//! axum HTTP + WebSocket server: serves the TS viewer and streams the timeline.
//!
//! The viewer bundle is **embedded into the binary** (via `rust-embed`) so a
//! release build of greenlane is a single self-contained executable — no assets
//! to ship. In debug builds rust-embed reads `web/dist` from disk at runtime,
//! so you can rebuild the frontend without recompiling Rust. `--web-dir` forces
//! serving from an arbitrary directory (e.g. when iterating outside the repo).
//!
//! Wire protocol (server → client, JSON text frames):
//!   { "type": "snapshot", "slices": [Slice, ...] }   // first frame on connect
//!   { "type": "slices",   "slices": [Slice, ...] }   // subsequent tail deltas
//!
//! Each client keeps a cursor; on a fixed timer the server sends the contiguous
//! tail since that cursor (naturally coalesced to ~30 frames/s regardless of
//! switch rate). Lossless — no broadcast lag, no resync storms.
//!
//! JSON is the v1 transport; swap to binary framing (postcard) when rates demand.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::db::{Db, Query, Reply};

/// How often a live session pushes a `head` (so the viewer follows + updates the
/// header). Recordings are static and get none.
const HEAD_TICK: Duration = Duration::from_millis(100);

/// Hard cap on slices returned per viewport window (memory bound).
const WINDOW_CAP: usize = 200_000;
/// Slow-span thresholds (ns): warn > 20ms, red > 50ms (mirrors the renderer).
const WARN_NS: u64 = 20_000_000;
const RED_NS: u64 = 50_000_000;

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
}

/// The built viewer bundle, embedded at compile time.
#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
struct Assets;

/// Run the viewer server until `shutdown` resolves.
pub async fn serve(
    db: Db,
    pid: i32,
    detached: Arc<AtomicBool>,
    source: Option<Source>,
    addr: SocketAddr,
    web_dir: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = AppState {
        db,
        pid,
        detached,
        source,
    };
    let mut app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/detach", post(detach_handler));

    app = match &web_dir {
        Some(dir) => app.fallback_service(ServeDir::new(dir)),
        None => app.fallback(static_handler),
    };

    // CorsLayer lets the bun/vite dev server (different origin) reach /ws.
    let app = app.layer(CorsLayer::permissive()).with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    match &web_dir {
        Some(dir) => {
            tracing::info!(url = %format!("http://{addr}"), assets = %dir.display(), "viewer ready")
        }
        None => {
            tracing::info!(url = %format!("http://{addr}"), assets = "embedded", "viewer ready")
        }
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Serve an embedded asset, falling back to index.html for SPA client routing.
async fn static_handler(uri: Uri) -> Response {
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

/// POST /detach — stop collecting so the target removes its trace hook.
async fn detach_handler(State(st): State<AppState>) -> StatusCode {
    st.detached.store(true, Ordering::SeqCst);
    StatusCode::OK
}

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
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
        "totalSlices": st.db.total(),
        "bytes": st.db.bytes(),
    });
    if send_json(&mut socket, &meta).await.is_err() {
        return;
    }

    let mut was_detached = st.detached.load(Ordering::SeqCst);
    let mut tick = tokio::time::interval(HEAD_TICK);

    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    // The viewer drives data flow with viewport/slowlog/stats requests.
                    Some(Ok(Message::Text(t))) => {
                        if let Some(reply) = handle_request(&st, &t).await
                            && send_json(&mut socket, &reply).await.is_err()
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
                if !st.detached.load(Ordering::SeqCst) {
                    let head = serde_json::json!({
                        "type": "head",
                        "spanNs": st.db.span(),
                        "totalSlices": st.db.total(),
                        "bytes": st.db.bytes(),
                    });
                    if send_json(&mut socket, &head).await.is_err() {
                        break;
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

/// Parse a client request and run the matching DB query, returning the JSON reply.
async fn handle_request(st: &AppState, text: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    match v.get("type")?.as_str()? {
        "viewport" => {
            let t0 = v.get("t0")?.as_u64()?;
            let t1 = v.get("t1")?.as_u64()?;
            let buckets = v.get("px").and_then(|x| x.as_u64()).unwrap_or(1000) as usize;
            let q = Query::Window {
                t0,
                t1,
                cap: WINDOW_CAP,
                buckets: buckets.clamp(1, 4096),
            };
            match st.db.query(q).await {
                Ok(Reply::Window {
                    slices,
                    cpu,
                    tracks,
                    gc,
                    visible,
                    capped,
                }) => Some(serde_json::json!({
                    "type": "window", "t0": t0, "t1": t1,
                    "slices": slices, "cpu": cpu, "tracks": tracks, "gc": gc,
                    "counts": { "visible": visible, "total": st.db.total() },
                    "capped": capped, "spanNs": st.db.span(), "bytes": st.db.bytes(),
                })),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "window query failed");
                    None
                }
            }
        }
        "slowlog" => {
            let level = v.get("level").and_then(|x| x.as_str()).unwrap_or("all");
            let sort_dur = v.get("sort").and_then(|x| x.as_str()) == Some("dur");
            let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(500) as usize;
            let q = Query::Slowlog {
                warn_ns: WARN_NS,
                red_ns: RED_NS,
                red_only: level == "red",
                sort_dur,
                limit,
            };
            match st.db.query(q).await {
                Ok(Reply::Slowlog(rows)) => {
                    Some(serde_json::json!({ "type": "slowlog", "rows": rows }))
                }
                _ => None,
            }
        }
        "stats" => {
            let t0 = v.get("t0").and_then(|x| x.as_u64()).unwrap_or(0);
            // i64::MAX, not u64::MAX — the DB casts times to i64, where u64::MAX
            // wraps to -1 and would exclude every row.
            let t1 = v.get("t1").and_then(|x| x.as_u64()).unwrap_or(i64::MAX as u64);
            match st.db.query(Query::Stats { t0, t1 }).await {
                Ok(Reply::Stats { p50, p95, p99 }) => Some(serde_json::json!({
                    "type": "stats", "p50": p50, "p95": p95, "p99": p99,
                })),
                _ => None,
            }
        }
        _ => None,
    }
}
