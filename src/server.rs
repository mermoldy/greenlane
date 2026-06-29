//! axum HTTP + WebSocket server: serves the TS viewer and streams the timeline.
//!
//! The viewer bundle is **embedded into the binary** (via `rust-embed`) so a
//! release build of greenlane is a single self-contained executable — no assets
//! to ship. In debug builds rust-embed reads `web/dist` from disk at runtime,
//! so you can rebuild the frontend without recompiling Rust. `--web-dir` forces
//! serving from an arbitrary directory (e.g. when iterating outside the repo).
//!
//! Wire protocol (over one WebSocket). The viewer is **request-driven**: it asks
//! for exactly the viewport it needs and the server answers from the DB.
//!   client → server (JSON text): `{type:"viewport",t0,t1,px,req}`, `{type:"slowlog",…}`,
//!     `{type:"stats",…}`.
//!   server → client: a `viewport` reply is a compact **binary columnar frame**
//!     (see `encode_window` / the TS `decodeWindow`) — typed-array columns + a small
//!     JSON header; `slowlog`/`stats`/`meta`/`head`/`status` are JSON text.
//! On connect the server sends a `meta` frame; while live it pushes a small `head`
//! (span/total/bytes) on a timer so the viewer follows the edge. No server-side
//! push of execution data — the client pulls each window, so there's no broadcast lag.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::Thresholds;
use crate::db::{Db, Query, Reply};
use crate::sysinfo::SysInfo;

/// How often a live session pushes a `head` (so the viewer follows + updates the
/// header). Recordings are static and get none.
const HEAD_TICK: Duration = Duration::from_millis(100);

/// How often the live-attach stats monitor logs throughput and re-evaluates whether
/// the tracer has stalled. One interval with zero new executions (while live) marks
/// the tracer stalled.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// Periodic stream-stats + tracer-stall monitor for a live attach. Logs per-interval
/// throughput (executions/s, bytes/s) and flips `tracer_stalled` when the session is
/// live but executions stopped advancing — i.e. the target's greenlet switch hook
/// went quiet even though the stream (e.g. GC frames) may still be flowing. Clears the
/// moment executions resume. Recordings don't run this (they're static).
fn spawn_stats_monitor(db: Db, detached: Arc<AtomicBool>, tracer_stalled: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(STATS_INTERVAL);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let dt = STATS_INTERVAL.as_secs_f64();
        let (mut prev_total, mut prev_bytes) = (db.total(), db.bytes());
        loop {
            iv.tick().await;
            let (total, bytes) = (db.total(), db.bytes());
            let live = !detached.load(Ordering::SeqCst);
            let new_execs = total.saturating_sub(prev_total);
            let new_bytes = bytes.saturating_sub(prev_bytes);
            // Live + some data captured + nothing new this interval → the switch hook
            // has gone quiet (a still-flowing byte count means the stream is otherwise
            // alive, the classic silent stall; zero bytes means fully quiet).
            let stalled = live && total > 0 && new_execs == 0;
            let was = tracer_stalled.swap(stalled, Ordering::SeqCst);
            if stalled && !was {
                tracing::warn!(
                    executions = total,
                    stream_alive = new_bytes > 0,
                    "tracer stalled — no new executions; greenlet switch hook may have stopped"
                );
            } else if !stalled && was {
                tracing::info!(executions = total, "tracer resumed");
            }
            tracing::info!(
                executions = total,
                new_executions = new_execs,
                exec_per_s = (new_execs as f64 / dt).round() as u64,
                bytes,
                kib_per_s = (new_bytes as f64 / dt / 1024.0).round() as u64,
                live,
                stalled,
                "stream stats"
            );
            (prev_total, prev_bytes) = (total, bytes);
        }
    });
}

/// Hard cap on executions returned per viewport window (memory bound).
const WINDOW_CAP: usize = 200_000;

/// Provenance of an opened recording, surfaced to the viewer for context.
#[derive(Clone)]
pub struct Source {
    /// Path the recording was loaded from.
    pub file: String,
    /// On-disk size of that file, in bytes.
    pub bytes: u64,
}

/// Shared server state.
#[derive(Clone)]
struct AppState {
    db: Db,
    /// Target PID we attached to.
    pid: i32,
    /// Set by /detach: stops collection so the target self-uninstalls its hook.
    detached: Arc<AtomicBool>,
    /// Set when serving a `.glr` recording (vs a live attach); `None` when live.
    source: Option<Source>,
    /// Trace mode this session captured with — `"off"`, `"slow"`, or `"all"`
    /// (`--include-traces`). `None` for recordings, where the mode isn't known (the
    /// viewer infers per-execution from whether the stack is present). Drives the detail
    /// panel's per-execution "why no full stack" copy.
    trace_mode: Option<&'static str>,
    /// Host/process/runtime introspection + scheduler-lag (live attaches only).
    sys: Option<Arc<SysInfo>>,
    /// Set by the stats monitor when the session is live but executions have stopped
    /// advancing (the target's switch hook went quiet while the stream is otherwise
    /// alive). Surfaced in `head` + `/healthz` so the viewer stops chasing the edge.
    tracer_stalled: Arc<AtomicBool>,
    /// Warn/block execution-duration thresholds (slow-log filter + sent to the viewer).
    thresholds: Thresholds,
    /// Per-session secret required to reach `/ws`, `/info`, `/detach`. Supplied via
    /// the capability URL greenlane prints (`?token=…`); loading that URL sets a
    /// same-origin cookie the browser then sends automatically. Without it, a host
    /// reachable over the network can't read the timeline or POST `/detach`.
    token: Arc<str>,
    /// Whether token auth is enforced. Off in `--web-dir` dev mode, where the bun
    /// dev server is a different origin (cross-origin → no same-origin cookie) and
    /// CORS is already permissive for local iteration.
    auth_enabled: bool,
}

impl AppState {
    /// Whether a request carries the right token, via the `?token=` query param or
    /// the `gl_token` cookie (set when the capability URL is opened).
    fn authed(&self, raw_query: Option<&str>, headers: &header::HeaderMap) -> bool {
        if !self.auth_enabled {
            return true;
        }
        let provided = token_from_query(raw_query).or_else(|| token_from_cookie(headers));
        // Length-then-bytes compare; the token is 128 bits of CSPRNG, so a timing
        // side-channel isn't a practical threat here.
        provided.as_deref() == Some(&*self.token)
    }
}

/// Extract `token` from a raw query string. The token is hex, so no percent-decode.
fn token_from_query(raw: Option<&str>) -> Option<String> {
    raw?.split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(|s| s.to_string())
}

/// Extract the `gl_token` cookie value from request headers.
fn token_from_cookie(headers: &header::HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix("gl_token="))
        .map(|s| s.to_string())
}

/// 16 random bytes from the OS CSPRNG as lowercase hex (the session token).
fn random_token() -> String {
    // Extremely unlikely to fail; fall back to a less-ideal but non-empty token.
    crate::random_hex16().unwrap_or_else(|_| format!("{:x}", std::process::id() as u64))
}

/// The built viewer bundle, embedded at compile time.
#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
struct Assets;

/// Run the viewer server until `shutdown` resolves.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    db: Db,
    pid: i32,
    detached: Arc<AtomicBool>,
    source: Option<Source>,
    trace_mode: Option<&'static str>,
    sys: Option<Arc<SysInfo>>,
    thresholds: Thresholds,
    addr: SocketAddr,
    web_dir: Option<PathBuf>,
    no_auth: bool,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    // Token auth is on by default; `--no-auth` opts out, and `--web-dir` dev mode
    // (cross-origin → no same-origin cookie) can't use it.
    let auth_enabled = web_dir.is_none() && !no_auth;
    let token: Arc<str> = Arc::from(random_token());
    let tracer_stalled = Arc::new(AtomicBool::new(false));
    // Live attaches (no recording source) get a periodic stats monitor: it logs
    // stream throughput and flips `tracer_stalled` when executions stop advancing
    // while the session is live (the target's switch hook went quiet).
    if source.is_none() {
        spawn_stats_monitor(db.clone(), detached.clone(), tracer_stalled.clone());
    }
    let state = AppState {
        db,
        pid,
        detached,
        source,
        trace_mode,
        sys,
        tracer_stalled,
        thresholds,
        token: token.clone(),
        auth_enabled,
    };
    let mut app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/info", get(info_handler))
        .route("/healthz", get(health_handler))
        .route("/detach", post(detach_handler));

    app = match &web_dir {
        Some(dir) => app.fallback_service(ServeDir::new(dir)),
        None => app.fallback(static_handler),
    };

    // Permissive CORS only in frontend-dev mode (--web-dir), where the bun/vite
    // dev server is a different origin. In normal use the viewer is same-origin
    // (embedded assets) and a per-session token gates /ws|/info|/detach (see
    // AppState::authed), so binding beyond localhost doesn't expose an open control
    // endpoint — a caller without the token (from the printed capability URL) is
    // rejected. (Dev mode disables the token; don't expose --web-dir publicly.)
    let app = if web_dir.is_some() {
        app.layer(CorsLayer::permissive())
    } else {
        app
    };
    let app = app.with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    // With auth on, print the capability URL (the token authorizes /ws, /info,
    // /detach); with auth off, the bare URL is all that's needed.
    let url = if auth_enabled {
        format!("http://{addr}/?token={token}")
    } else {
        format!("http://{addr}")
    };
    // Where the viewer's assets come from: the embedded bundle (normal) or a
    // directory on disk (`--web-dir` dev mode).
    let assets = match &web_dir {
        Some(dir) => format!("{} (--web-dir)", dir.display()),
        None => "embedded".to_string(),
    };
    if auth_enabled {
        tracing::info!(url = %url, assets, "viewer ready — open this URL");
    } else {
        let reason = if web_dir.is_some() {
            "--web-dir dev mode"
        } else {
            "--no-auth"
        };
        tracing::warn!(url = %url, assets, reason, "viewer ready — auth disabled; anyone who can reach this address has full access to the timeline and /detach");
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Serve an embedded asset, falling back to index.html for SPA client routing.
/// Gated on the session token: an unauthenticated request (no valid `?token=` and
/// no `gl_token` cookie) gets a **403 page** instead of the viewer, so a stray
/// visitor sees a clear explanation rather than a viewer that can't connect. When
/// the request carries a valid `?token=` (the capability URL greenlane prints) it
/// also sets a same-origin `gl_token` cookie so the viewer's subsequent `/ws`,
/// `/info`, `/detach` requests authenticate automatically.
async fn static_handler(
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if !st.authed(q.as_deref(), &headers) {
        return forbidden_page();
    }
    let mut resp = serve_asset(&uri);
    if token_from_query(q.as_deref()).as_deref() == Some(&*st.token)
        && let Ok(v) = header::HeaderValue::from_str(&format!(
            "gl_token={}; Path=/; HttpOnly; SameSite=Strict",
            st.token
        ))
    {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

/// A minimal self-contained 403 page (no external assets, so it renders under the
/// viewer's strict same-origin loading). Shown when the viewer is opened without a
/// valid session token.
fn forbidden_page() -> Response {
    // The GIF is embedded as a base64 `data:` URI (built from src/forbidden.gif),
    // so the page stays fully self-contained — no external host, nothing for the
    // strict same-origin loading / CSP to block.
    const GIF: &str = include_str!("forbidden.gif.datauri");
    let body = format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>403 — greenlane</title>
<style>
  html{{color-scheme:dark light}}
  body{{font:15px/1.6 ui-sans-serif,system-ui,sans-serif;max-width:34rem;margin:14vh auto;padding:0 1.25rem;text-align:center}}
  h1{{font-size:1.4rem;margin:0 0 .5rem}}
  code{{font-family:ui-monospace,Menlo,monospace;background:color-mix(in srgb,currentColor 12%,transparent);padding:.1em .35em;border-radius:4px}}
  img{{width:200px;max-width:60%;height:auto;border-radius:10px;margin:0 0 1rem}}
  .muted{{opacity:.7}}
</style>
<img src="{GIF}" alt="sad SpongeBob" width="319" height="317">
<h1>403 — Forbidden</h1>
<p>This greenlane viewer needs its session token.</p>
<p class="muted">Open the capability URL greenlane printed when it started — the one
ending in <code>?token=…</code>. The token is shown only in greenlane's own output,
so only you can reach this viewer.</p>
"#
    );
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from(body),
    )
        .into_response()
}

fn serve_asset(uri: &Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path).or_else(|| Assets::get("index.html")) {
        Some(content) => {
            let mime = mime_for(path);
            (
                [(header::CONTENT_TYPE, mime)],
                Body::from(content.data.into_owned()),
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            "viewer bundle not built — run `bun run build` in web/",
        )
            .into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

/// GET /healthz — unauthenticated liveness + tracing status for health checks /
/// quick triage. Exposes only counters (no timeline data), so it stays open for k8s
/// probes. `tracerStalled` flags a live session whose executions stopped advancing.
async fn health_handler(State(st): State<AppState>) -> Response {
    let live = !st.detached.load(Ordering::SeqCst);
    let stalled = st.tracer_stalled.load(Ordering::SeqCst);
    let body = serde_json::json!({
        "status": if live && stalled { "stalled" } else { "ok" },
        "pid": st.pid,
        "live": live,
        "tracerStalled": stalled,
        "executions": st.db.total(),
        "bytes": st.db.bytes(),
        "spanNs": st.db.span(),
        "recording": st.source.is_some(),
    });
    axum::Json(body).into_response()
}

/// POST /detach — stop collecting so the target removes its trace hook.
async fn detach_handler(
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> StatusCode {
    if !st.authed(q.as_deref(), &headers) {
        return StatusCode::FORBIDDEN;
    }
    st.detached.store(true, Ordering::SeqCst);
    StatusCode::OK
}

/// GET /info — host/process/runtime details + live scheduler-lag for the System
/// panel. Recordings (no live target) report what little they have, lag `null`.
async fn info_handler(
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if !st.authed(q.as_deref(), &headers) {
        return (StatusCode::FORBIDDEN, "missing or invalid session token").into_response();
    }
    let live = !st.detached.load(Ordering::SeqCst);
    let source = st
        .source
        .as_ref()
        .map(|s| serde_json::json!({ "file": s.file, "bytes": s.bytes }));
    let body = match &st.sys {
        Some(sys) => sys.to_json(live, source),
        None => serde_json::json!({
            "pid": st.pid,
            "live": live,
            "source": source,
            "tid": serde_json::Value::Null,
            "kernel": serde_json::Value::Null,
            "process": serde_json::Value::Null,
            "python": serde_json::Value::Null,
            "lag": serde_json::Value::Null,
        }),
    };
    axum::Json(body).into_response()
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if !st.authed(q.as_deref(), &headers) {
        return (StatusCode::FORBIDDEN, "missing or invalid session token").into_response();
    }
    ws.on_upgrade(move |socket| client(socket, st))
}

async fn client(mut socket: WebSocket, st: AppState) {
    // Session metadata: PID, epoch, live/detached, recording source, the fixed
    // global time origin, the full captured span + counts (so the viewer can
    // fit/follow and render the header without holding any timeline data).
    let meta = serde_json::json!({
        "type": "meta",
        "pid": st.pid,
        "epochMs": st.db.epoch(),
        "live": !st.detached.load(Ordering::SeqCst),
        "source": st.source.as_ref().map(|s| serde_json::json!({
            "file": s.file,
            "bytes": s.bytes,
        })),
        "originNs": st.db.origin(),
        "spanNs": st.db.span(),
        "totalExecutions": st.db.total(),
        "bytes": st.db.bytes(),
        // Live retention horizon (ns): in a live-view-only session old rows are
        // evicted past a cap; data before this point is gone (0 = nothing evicted).
        "retainedFromNs": st.db.retained_from(),
        // Span-duration thresholds (ns) so the viewer colors/labels match the
        // server's slow-log filter; configurable via --warn-ms/--block-ms.
        "warnNs": st.thresholds.warn_ns,
        "blockNs": st.thresholds.block_ns,
        // Trace mode (--include-traces): "off" | "slow" | "all", or null for
        // recordings (unknown). `traces` stays as a convenience bool (mode != off);
        // the viewer uses `traceMode` for the per-execution "why no full stack" copy.
        "traces": st.trace_mode.map(|m| m != "off"),
        "traceMode": st.trace_mode,
    });
    if send_json(&mut socket, &meta).await.is_err() {
        return;
    }
    tracing::debug!(
        total_executions = st.db.total(),
        span_ns = st.db.span(),
        live = !st.detached.load(Ordering::SeqCst),
        "viewer connected; sent meta"
    );

    let mut was_detached = st.detached.load(Ordering::SeqCst);
    let mut last_head = (0u64, 0u64, 0u64); // (span, total, bytes) last pushed
    let mut last_stalled = false; // last `stalled` flag pushed in a head
    let mut tick = tokio::time::interval(HEAD_TICK);

    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    // The viewer drives data flow with viewport/slowlog/stats requests.
                    Some(Ok(Message::Text(t))) => {
                        let started = Instant::now();
                        let reply = handle_request(&st, &t).await;
                        // One per viewport/slowlog/stats request — fires on every
                        // pan/zoom, so trace, not debug.
                        tracing::trace!(
                            req = %t.chars().take(80).collect::<String>(),
                            elapsed_ms = started.elapsed().as_secs_f64() * 1e3,
                            replied = reply.is_some(),
                            "served viewer request"
                        );
                        if let Some(reply) = reply
                            && socket.send(reply).await.is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(_)) => {} // ping/pong/binary — ignore
                    _ => break,        // closed or error
                }
            }
            _ = tick.tick() => {
                // Live sessions push a head so the viewer follows the edge and the
                // header (span/total/bytes) stays current; recordings are static.
                // Skip the push when nothing changed since last tick (idle target).
                if !st.detached.load(Ordering::SeqCst) {
                    let now = (st.db.span(), st.db.total(), st.db.bytes());
                    let stalled = st.tracer_stalled.load(Ordering::SeqCst);
                    // Push when the counters moved OR the stall flag flipped — so the
                    // viewer learns of a stall even if the stream went fully quiet
                    // (counters frozen) and would otherwise get no head.
                    if now != last_head || stalled != last_stalled {
                        last_head = now;
                        last_stalled = stalled;
                        let head = serde_json::json!({
                            "type": "head",
                            "spanNs": now.0,
                            "totalExecutions": now.1,
                            "bytes": now.2,
                            // Tracer stalled: live but executions stopped advancing — the
                            // viewer uses this to stop chasing the wall-clock edge.
                            "stalled": stalled,
                            // Retention horizon advances as old rows evict (live cap).
                            "retainedFromNs": st.db.retained_from(),
                            // R13: current hub-thread scheduler-lag rate (ms/s), or null
                            // where unsupported. The viewer plots it at the live edge
                            // (spanNs), so it aligns to the trace axis with no clock map.
                            "lagMsPerSec": st.sys.as_ref().and_then(|s| s.lag_rate_ms_s()),
                            // Hub-thread on-CPU rate (ms/s; /1000 = utilization). Lets
                            // the viewer keep the CPU band's live tail moving in the
                            // pending area, the same way it does for lag.
                            "cpuMsPerSec": st.sys.as_ref().and_then(|s| s.cpu_rate_ms_s()),
                        });
                        if send_json(&mut socket, &head).await.is_err() {
                            break;
                        }
                    }
                }
                // Notify the viewer when the session detaches.
                let d = st.detached.load(Ordering::SeqCst);
                if d != was_detached {
                    was_detached = d;
                    let s = serde_json::json!({ "type": "status", "live": !d });
                    if send_json(&mut socket, &s).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

async fn send_json(socket: &mut WebSocket, v: &serde_json::Value) -> Result<(), ()> {
    socket
        .send(Message::Text(v.to_string().into()))
        .await
        .map_err(|_| ())
}

/// Parse a client request and run the matching DB query, returning the reply
/// frame. The hot `window` reply is a compact **binary** frame (columnar typed
/// arrays + a small JSON header); the small/infrequent replies stay JSON text.
async fn handle_request(st: &AppState, text: &str) -> Option<Message> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    match v.get("type")?.as_str()? {
        "viewport" => {
            let t0 = v.get("t0")?.as_u64()?;
            let t1 = v.get("t1")?.as_u64()?;
            // Monotonic request id, echoed back so the viewer can drop superseded
            // replies (e.g. older windows arriving mid-pan).
            let req = v.get("req").and_then(|x| x.as_u64()).unwrap_or(0);
            // Live-follow append: when the viewer sends `from`, it already holds
            // everything up to its data frontier and only wants the new tail to
            // append (`from`/`gcFrom` are its row/GC frontiers — see Query::Tail).
            // Absent → a full window `[t0, t1]` it replaces. The reply frame is
            // identical bar an `append` flag echoed in the header.
            let append_from = v.get("from").and_then(|x| x.as_u64());
            let q = match append_from {
                Some(from) => Query::Tail {
                    from,
                    gc_from: v.get("gcFrom").and_then(|x| x.as_u64()).unwrap_or(from),
                    t1,
                    cap: WINDOW_CAP,
                },
                None => Query::Window {
                    t0,
                    t1,
                    cap: WINDOW_CAP,
                },
            };
            match st.db.query(q).await {
                Ok(Reply::Window {
                    start,
                    dur,
                    gid,
                    tracks,
                    gc,
                    visible,
                    capped,
                    sorted,
                    min_start,
                    max_start,
                    max_end,
                }) => {
                    debug_assert_eq!(visible, start.len());
                    Some(Message::Binary(
                        encode_window(
                            st,
                            req,
                            t0,
                            t1,
                            append_from.is_some(),
                            &start,
                            &dur,
                            &gid,
                            &tracks,
                            &gc,
                            capped,
                            sorted,
                            min_start,
                            max_start,
                            max_end,
                        )
                        .into(),
                    ))
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "window query failed");
                    None
                }
            }
        }
        "slowlog" => {
            let level = v.get("level").and_then(|x| x.as_str()).unwrap_or("all");
            let tier = match level {
                "warn" => crate::db::SlowTier::Warn,
                "block" => crate::db::SlowTier::Block,
                _ => crate::db::SlowTier::All,
            };
            let sort_dur = v.get("sort").and_then(|x| x.as_str()) == Some("dur");
            let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(500) as usize;
            let q = Query::Slowlog {
                warn_ns: st.thresholds.warn_ns,
                red_ns: st.thresholds.block_ns,
                tier,
                sort_dur,
                limit,
            };
            match st.db.query(q).await {
                // Serialize the `SlowRow`s straight to text (no intermediate
                // `serde_json::Value` tree) — this is polled with up to 5000 rows
                // every second while the panel is open, so the per-row allocations a
                // `json!` macro would build are worth avoiding.
                Ok(Reply::Slowlog { rows, total }) => Some(json_text(&SlowlogMsg {
                    ty: "slowlog",
                    rows: &rows,
                    total,
                })),
                _ => None,
            }
        }
        "stats" => {
            let t0 = v.get("t0").and_then(|x| x.as_u64()).unwrap_or(0);
            // i64::MAX, not u64::MAX — the DB casts times to i64, where u64::MAX
            // wraps to -1 and would exclude every row.
            let t1 = v
                .get("t1")
                .and_then(|x| x.as_u64())
                .unwrap_or(i64::MAX as u64);
            match st.db.query(Query::Stats { t0, t1 }).await {
                Ok(Reply::Stats { p50, p95, p99 }) => Some(json_text(&StatsMsg {
                    ty: "stats",
                    p50,
                    p95,
                    p99,
                })),
                _ => None,
            }
        }
        // Lazy per-execution detail: the window frame is render-only, so the viewer asks
        // for a hovered execution's func/task/stack here. `gid` + `startNs` identify the
        // execution (start is the viewer's f32-ms estimate, so the DB does a nearest
        // match within ±max(dur, 2ms) on that gid). The reply echoes gid+startNs so
        // the client only applies it if still hovering the same execution.
        "detail" => {
            let gid = v.get("gid")?.as_u64()?;
            let start_ns = v.get("startNs")?.as_u64()?;
            let dur_ns = v.get("durNs").and_then(|x| x.as_u64()).unwrap_or(0);
            match st
                .db
                .query(Query::Detail {
                    gid,
                    start_ns,
                    dur_ns,
                })
                .await
            {
                Ok(Reply::Detail { func, task, stack }) => Some(json_text(&DetailMsg {
                    ty: "detail",
                    gid,
                    start_ns,
                    func,
                    task,
                    stack,
                })),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Serialize a reply struct straight to a text WS frame — skips the intermediate
/// `serde_json::Value` tree the `json!` macro builds (per-row allocations on the
/// high-volume slowlog/window paths). Serialization of these plain structs can't
/// fail, but fall back to an empty frame rather than panicking if it ever does.
fn json_text<T: Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).unwrap_or_default().into())
}

#[derive(Serialize)]
struct SlowlogMsg<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    rows: &'a [crate::db::SlowRow],
    total: usize,
}

#[derive(Serialize)]
struct StatsMsg {
    #[serde(rename = "type")]
    ty: &'static str,
    p50: f64,
    p95: f64,
    p99: f64,
}

#[derive(Serialize)]
struct DetailMsg {
    #[serde(rename = "type")]
    ty: &'static str,
    gid: u64,
    #[serde(rename = "startNs")]
    start_ns: u64,
    func: String,
    task: String,
    stack: String,
}

/// Encode the `window` reply as a binary frame:
///
/// ```text
///   u32 LE headerLen | header JSON (utf-8) | pad→4 | columns…
/// ```
///
/// The header carries the small/structured parts (counts, span, tracks, gc). The
/// per-execution data is three parallel typed-array columns — `startMs` f32, `durMs`
/// f32, `trackIdx` u32 — **12 bytes/execution, render-only**: no func/task/stack and
/// no string dictionary. A execution's func/task/stack are fetched lazily on hover via
/// the `detail` request (see handle_request), so wide windows stay small even
/// under `--include-traces=all`. CPU bins aren't sent — the client derives them.
/// JSON header of the binary `window` frame. Serialized from borrowed slices so the
/// `tracks`/`gc` arrays aren't cloned into a `serde_json::Value` first.
#[derive(Serialize)]
struct WindowHeader<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    req: u64,
    t0: u64,
    t1: u64,
    n: usize,
    counts: WindowCounts,
    capped: bool,
    /// Whether the timeline is in start-sorted order; the viewer only uses the append
    /// fast path while true (see `Reply::Window::sorted`).
    sorted: bool,
    #[serde(rename = "minStart")]
    min_start: u64,
    /// Max start of the returned rows — the viewer's next live-follow data frontier.
    #[serde(rename = "maxStart")]
    max_start: u64,
    #[serde(rename = "maxEnd")]
    max_end: u64,
    #[serde(rename = "spanNs")]
    span_ns: u64,
    bytes: u64,
    #[serde(rename = "retainedFromNs")]
    retained_from_ns: u64,
    /// True when this frame is the new tail of a live-follow `Tail` query (the viewer
    /// appends its rows); false for a full window (the viewer replaces).
    append: bool,
    tracks: &'a [crate::db::TrackRun],
    gc: &'a [crate::store::GcEvent],
}

#[derive(Serialize)]
struct WindowCounts {
    visible: usize,
    total: u64,
}

#[allow(clippy::too_many_arguments)]
fn encode_window(
    st: &AppState,
    req: u64,
    t0: u64,
    t1: u64,
    append: bool,
    start_ns: &[i64],
    dur_ns: &[i64],
    gid: &[u64],
    tracks: &[crate::db::TrackRun],
    gc: &[crate::store::GcEvent],
    capped: bool,
    sorted: bool,
    min_start: u64,
    max_start: u64,
    max_end: u64,
) -> Vec<u8> {
    use std::collections::HashMap;
    let n = start_ns.len();

    let gid_idx: HashMap<u64, u32> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.gid, i as u32))
        .collect();

    // Render-only frame: each execution is just start/dur/track (12 bytes). The
    // func/task/stack an execution carries are NOT shipped here — the viewer fetches
    // them lazily, per execution, via the `detail` request on hover (see
    // handle_request). This keeps wide windows small and, crucially, drops the
    // per-window string dictionary that exploded under `--include-traces=all`.
    let mut start = Vec::with_capacity(n);
    let mut dur = Vec::with_capacity(n);
    let mut trk = Vec::with_capacity(n);
    for i in 0..n {
        // Encode `start` as ms **relative to the window's t0**, not the global
        // origin: offsets within a window are small, so f32 keeps sub-ms precision
        // even hours into a capture (an absolute offset would lose it). The viewer
        // adds back `t0 - origin` in f64 to recover the absolute position. Spans
        // straddling t0 are negative — fine. (See wire.ts / timeline.ts.)
        start.push(((start_ns[i] - t0 as i64) as f32) / 1e6);
        dur.push(dur_ns[i] as f32 / 1e6);
        trk.push(gid_idx.get(&gid[i]).copied().unwrap_or(0));
    }

    // Serialize the header straight from borrowed slices (no `serde_json::Value`
    // tree, so `tracks`/`gc` aren't cloned into a `Value` per element first).
    let header = WindowHeader {
        ty: "window",
        req,
        t0,
        t1,
        n,
        counts: WindowCounts {
            visible: n,
            total: st.db.total(),
        },
        capped,
        sorted,
        // Absolute ns bounds of the executions actually returned (0 if empty), so the
        // viewer records the range it genuinely has rather than the requested one —
        // which overstates coverage when `capped` truncated an edge. `max_start` is
        // the viewer's next live-follow data frontier.
        min_start,
        max_start,
        max_end,
        span_ns: st.db.span(),
        bytes: st.db.bytes(),
        // Live retention horizon (ns): data before this was evicted (0 = none).
        retained_from_ns: st.db.retained_from(),
        append,
        tracks,
        gc,
    };
    let hbytes = serde_json::to_vec(&header).unwrap_or_default();

    let mut buf = Vec::with_capacity(8 + hbytes.len() + n * 12);
    buf.extend_from_slice(&(hbytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&hbytes);
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
    for v in &start {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in &dur {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in &trk {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    //! The binary `window` frame is the server↔viewer contract. This asserts its
    //! layout (u32 headerLen | JSON header | pad→4 | 6 typed-array columns) so it
    //! stays in lockstep with the TS `decodeWindow`. Run with `cargo test`.
    use super::*;
    use crate::db::{Db, TrackRun};
    use crate::store::{Execution, GcEvent};

    fn app_state(db: Db) -> AppState {
        AppState {
            db,
            pid: 1,
            detached: Arc::new(AtomicBool::new(false)),
            source: None,
            trace_mode: Some("all"),
            sys: None,
            tracer_stalled: Arc::new(AtomicBool::new(false)),
            thresholds: Thresholds {
                warn_ns: 20_000_000,
                block_ns: 50_000_000,
            },
            token: Arc::from("test-token"),
            auth_enabled: false,
        }
    }

    #[test]
    fn encode_window_layout_matches_decoder() {
        let db = Db::spawn(None).unwrap();
        let executions = vec![
            Execution {
                gid: 10,
                start: 0,
                dur: 5_000_000,
                name: "Greenlet-1".into(),
                func: "app.py:a:1".into(),
                task: "t".into(),
                stack: "app.py:a:1".into(),
            },
            Execution {
                gid: 20,
                start: 5_000_000,
                dur: 1_000_000,
                name: "Hub".into(),
                func: "app.py:a:1".into(),
                task: "".into(),
                stack: "".into(),
            },
        ];
        db.ingest_executions(executions.clone()); // sets origin/total
        // Render-only columns the window reply now carries (ns), 1:1 by row.
        let start_ns: Vec<i64> = executions.iter().map(|e| e.start as i64).collect();
        let dur_ns: Vec<i64> = executions.iter().map(|e| e.dur as i64).collect();
        let gid: Vec<u64> = executions.iter().map(|e| e.gid).collect();
        let tracks = vec![
            TrackRun {
                gid: 10,
                name: "Greenlet-1".into(),
                is_hub: false,
                run_ns: 5_000_000,
            },
            TrackRun {
                gid: 20,
                name: "Hub".into(),
                is_hub: true,
                run_ns: 1_000_000,
            },
        ];
        let gc = vec![GcEvent {
            start: 1,
            dur: 2,
            generation: 0,
            collected: 4,
        }];
        let st = app_state(db);

        let buf = encode_window(
            &st, 0, 0, 6_000_000, false, &start_ns, &dur_ns, &gid, &tracks, &gc, false, true, 0,
            5_000_000, 6_000_000,
        );

        // Header: u32 LE length prefix, then JSON.
        let hlen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let header: serde_json::Value =
            serde_json::from_slice(&buf[4..4 + hlen]).expect("header is valid JSON");
        assert_eq!(header["n"], 2);
        assert_eq!(header["type"], "window");
        assert_eq!(header["capped"], false);
        assert_eq!(header["tracks"].as_array().unwrap().len(), 2);
        assert_eq!(header["gc"].as_array().unwrap().len(), 1);
        // Render-only frame: no func/task/stack dictionary (fetched lazily via the
        // `detail` request), so the header carries no `dict`.
        assert!(header.get("dict").is_none());

        // Columns start 4-byte aligned and there are exactly three: startMs f32,
        // durMs f32, trackIdx u32 — 12 bytes/execution.
        let off = (4 + hlen + 3) & !3;
        assert_eq!(buf.len(), off + 2 /*n*/ * 4 /*bytes*/ * 3 /*cols*/);

        // First column is startMs (f32, relative to the window t0=0): [0.0, 5.0].
        let start0 = f32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        let start1 = f32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
        assert_eq!(start0, 0.0);
        assert_eq!(start1, 5.0);
    }
}
