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

use crate::store::Store;

/// How often each viewer is sent the tail of new slices.
const TICK: Duration = Duration::from_millis(33);

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
    store: Arc<Store>,
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
    store: Arc<Store>,
    pid: i32,
    detached: Arc<AtomicBool>,
    source: Option<Source>,
    addr: SocketAddr,
    web_dir: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = AppState {
        store,
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
    // Session metadata: PID, wall-clock epoch of trace t0 (for absolute time
    // modes; null if unknown), and current live/detached status.
    let meta = serde_json::json!({
        "type": "meta",
        "pid": st.pid,
        "epochMs": st.store.epoch(),
        "live": !st.detached.load(Ordering::SeqCst),
        // Provenance for opened recordings (file name + on-disk size); null when
        // this is a live attach, so the viewer can distinguish the two.
        "source": st.source.as_ref().map(|s| serde_json::json!({
            "file": s.file,
            "bytes": s.bytes,
        })),
        // Raw event-stream bytes processed so far (live data volume).
        "bytes": st.store.bytes(),
    });
    if socket
        .send(Message::Text(meta.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut cursor = 0usize;
    let mut gc_cursor = 0usize;
    let mut first = true;
    let mut was_detached = st.detached.load(Ordering::SeqCst);
    let mut tick = tokio::time::interval(TICK);

    loop {
        tokio::select! {
            // Detect close / process control frames by polling the socket.
            inbound = socket.recv() => {
                if inbound.is_none() { break; } // closed
            }
            _ = tick.tick() => {
                let (batch, len) = st.store.delta(cursor);
                cursor = len;
                // The first frame always goes out (even if empty) so the client
                // resets; later frames only when there's new data.
                if !batch.is_empty() || first {
                    let kind = if first { "snapshot" } else { "slices" };
                    first = false;
                    // Piggyback the running byte total so the header's live data
                    // volume updates in step with the timeline.
                    let msg = serde_json::json!({
                        "type": kind, "slices": batch, "bytes": st.store.bytes(),
                    });
                    if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                        break;
                    }
                }
                // GC pauses (global stalls), streamed the same way.
                let (gc, gc_len) = st.store.gc_delta(gc_cursor);
                gc_cursor = gc_len;
                if !gc.is_empty() {
                    let msg = serde_json::json!({ "type": "gc", "events": gc });
                    if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                        break;
                    }
                }
                // Notify the viewer when the session detaches.
                let d = st.detached.load(Ordering::SeqCst);
                if d != was_detached {
                    was_detached = d;
                    let s = serde_json::json!({ "type": "status", "live": !d });
                    if socket.send(Message::Text(s.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}
