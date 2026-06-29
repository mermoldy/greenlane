//! Embedded Polars-backed timeline store.
//!
//! All captured executions live in an in-memory Polars `DataFrame` (columnar Arrow,
//! pure Rust — no C++/system deps, links cleanly with musl), so neither the Rust
//! process nor the browser holds the timeline as a fat `Vec<Execution>`. A single
//! **DB-owning thread** serializes ingest and queries over a command channel
//! (a `DataFrame` isn't shared across threads here); ingest appends chunks via
//! `vstack`, queries (viewport window, CPU rollup, slow log, percentiles) run on
//! demand and reply over a tokio oneshot.
//!
//! The viewer selects only the visible range (see [`Query::Window`]); derived
//! views that used to scan the full client-side dataset (slow log, CPU, p95) are
//! now lazy Polars/SQL-style queries here, over the full data, regardless of what
//! is on screen. Note: Polars is in-memory (no disk spill) — see plan tradeoffs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use polars::prelude::*;
use serde::Serialize;
use tokio::sync::oneshot;
use tracing::{debug, error, trace};

use crate::store::{Execution, GcEvent};

/// Sentinel for "origin not yet observed".
const ORIGIN_UNSET: u64 = u64::MAX;
/// Append the ingest buffer to the DataFrame once it reaches this many rows…
const FLUSH_ROWS: usize = 16_384;
/// …or after this long with no command (keeps queries seeing fresh data).
const FLUSH_IDLE: Duration = Duration::from_millis(50);
/// Off-thread reads (slowlog/stats) and file flushes run on a small fixed pool so
/// many viewers or rapid polling can't spawn an unbounded number of OS threads.
/// Jobs beyond this many in flight queue until a worker frees up.
const READ_WORKERS: usize = 4;

// ── Query / reply types (also the wire shape — hence Serialize) ─────────────

/// A read request handed to the DB thread.
pub enum Query {
    /// Executions overlapping `[t0, t1]` ns, plus track/GC rollups for that window.
    Window {
        t0: u64,
        t1: u64,
        /// Hard cap on returned executions (memory bound); `capped` flags truncation.
        cap: usize,
    },
    /// The *new tail* of the timeline for live follow: executions whose START is in
    /// `(from, t1)` (NOT an overlap test), plus track/GC rollups for that slice. The
    /// viewer appends these to its buffers instead of re-fetching the whole window.
    ///
    /// `from` is the viewer's **data frontier** — the max start it already holds — NOT
    /// the requested window edge (which, while following, runs *ahead* of the data by
    /// the arrival lag, so a window-edge cursor would forever ask for an empty future
    /// and never see the rows filling in behind it). Executions are immutable closed
    /// intervals delivered in start order, so "start > from" yields exactly the rows
    /// the viewer lacks — no dup, no gap. `gc_from` is the same idea for GC (a separate
    /// frontier, since GC pauses are sparse and lag the row frontier). Replies as
    /// [`Reply::Window`], with `min_start`/`max_start`/`max_end` over the new rows.
    Tail {
        from: u64,
        gc_from: u64,
        t1: u64,
        /// Hard cap on returned executions (memory bound); `capped` flags truncation.
        cap: usize,
    },
    /// Newest slow executions (non-Hub), filtered + ordered for the slow-log panel.
    Slowlog {
        warn_ns: u64,
        red_ns: u64,
        tier: SlowTier,
        sort_dur: bool,
        limit: usize,
    },
    /// Duration percentiles (non-Hub) over `[t0, t1]` (whole timeline if full range).
    Stats { t0: u64, t1: u64 },
    /// On-demand per-execution detail (func/task/stack) for one execution, looked up
    /// by greenlet and start. The window frame is render-only (start/dur/track) and
    /// no longer ships these, so the viewer fetches them lazily on hover/click.
    /// `start_ns` is the viewer's estimate (its start column is f32 ms), so the lookup
    /// is a nearest-match within `±max(dur_ns, 2ms)` on that gid.
    Detail {
        gid: u64,
        start_ns: u64,
        dur_ns: u64,
    },
    /// Whole-capture aggregates for the headless `analyze` report: distinct greenlets,
    /// hub vs non-hub run totals, slow-execution counts, the top greenlets by run time, the
    /// hottest non-hub functions, and GC totals by generation. `top` bounds the two
    /// ranked lists.
    Summary {
        warn_ns: u64,
        red_ns: u64,
        top: usize,
    },
}

/// Which slow-log tier to return — selected server-side so the display `limit`
/// never hides matching rows of the requested tier (filtering `warn` *after* a
/// limited page could miss warn executions when blocks dominate the newest rows).
#[derive(Clone, Copy, PartialEq)]
pub enum SlowTier {
    /// Everything at/over the warn threshold (warn + block).
    All,
    /// Only the warn band: `warn <= dur < block`.
    Warn,
    /// Only the block band: `dur >= block`.
    Block,
}

/// Per-track run total over the queried window (drives sort + labels).
#[derive(Serialize)]
pub struct TrackRun {
    pub gid: u64,
    pub name: String,
    #[serde(rename = "isHub")]
    pub is_hub: bool,
    #[serde(rename = "runNs")]
    pub run_ns: u64,
}

/// One slow-log row.
#[derive(Serialize)]
pub struct SlowRow {
    pub start: u64,
    pub dur: u64,
    pub gid: u64,
    pub name: String,
    pub func: String,
    /// 1 = warn (> warn_ns), 2 = slow (> red_ns).
    pub level: u8,
}

/// One greenlet's whole-capture run total (for the `analyze` "top greenlets" list).
#[derive(Serialize)]
pub struct GreenletAgg {
    pub gid: u64,
    pub name: String,
    #[serde(rename = "isHub")]
    pub is_hub: bool,
    #[serde(rename = "runNs")]
    pub run_ns: u64,
    pub executions: u64,
}

/// One function's whole-capture run total (for the `analyze` "hottest functions"
/// list — non-Hub only). `func` is the `file.py:qualname:lineno` leaf label.
#[derive(Serialize)]
pub struct FuncAgg {
    pub func: String,
    #[serde(rename = "totalNs")]
    pub total_ns: u64,
    pub count: u64,
    #[serde(rename = "maxNs")]
    pub max_ns: u64,
}

/// GC totals for one generation (for the `analyze` "GC pressure" section).
#[derive(Serialize)]
pub struct GcGen {
    #[serde(rename = "gen")]
    pub generation: i64,
    pub count: u64,
    #[serde(rename = "totalNs")]
    pub total_ns: u64,
}

/// The DB thread's reply to a [`Query`].
pub enum Reply {
    Window {
        /// Render-only columns, 1:1 by row: `start`/`dur` in ns, `gid` the greenlet
        /// id (mapped to a track index by `encode_window`). The window frame is
        /// render-only, so func/task/stack are NOT materialized here — the viewer
        /// fetches them lazily per execution via [`Query::Detail`]. Keeping these as
        /// bare numeric columns avoids cloning four `String`s per row (up to
        /// WINDOW_CAP of them) on every pan/zoom, only to throw them away.
        start: Vec<i64>,
        dur: Vec<i64>,
        gid: Vec<u64>,
        tracks: Vec<TrackRun>,
        gc: Vec<GcEvent>,
        visible: usize,
        capped: bool,
        /// Whether the timeline is currently in start-sorted (single-cooperative-thread)
        /// order. Live-follow append relies on monotonic start arrival; once this is
        /// false (concurrent hubs → out-of-order/overlapping spans) the viewer must
        /// fall back to full windows, since a start-frontier cursor would skip a
        /// later-arriving span whose start precedes the frontier.
        sorted: bool,
        /// Absolute ns bounds of the executions actually returned (`0` when empty).
        /// `min_start`/`max_end` let the viewer record the range it truly has (when
        /// `capped` truncates an edge, the requested range overstates coverage);
        /// `max_start` is the viewer's next live-follow data frontier (the `from` for
        /// its next [`Query::Tail`]).
        min_start: u64,
        max_start: u64,
        max_end: u64,
    },
    Slowlog {
        rows: Vec<SlowRow>,
        /// Total matching slow executions (before the display limit) — for the badge.
        total: usize,
    },
    Stats {
        p50: f64,
        p95: f64,
        p99: f64,
    },
    /// Per-execution detail for one execution (empty strings if no row matched the lookup).
    Detail {
        func: String,
        task: String,
        stack: String,
    },
    Summary {
        /// Distinct greenlet (gid) count over the whole capture.
        greenlets: u64,
        /// Total run time on Hub/scheduler greenlets (ns).
        hub_run_ns: u64,
        /// Total run time on non-Hub greenlets (ns) — the app's actual work.
        nonhub_run_ns: u64,
        /// Non-Hub executions at/over the warn threshold.
        warn_count: u64,
        /// Non-Hub executions at/over the block threshold.
        block_count: u64,
        /// Top greenlets by run time, longest first (bounded by `top`).
        top_greenlets: Vec<GreenletAgg>,
        /// Hottest non-Hub functions by total run time (bounded by `top`).
        top_funcs: Vec<FuncAgg>,
        /// Total GC pauses, total GC pause time (ns), and the per-generation split.
        gc_count: u64,
        gc_total_ns: u64,
        gc_by_gen: Vec<GcGen>,
    },
}

enum Cmd {
    Executions(Vec<Execution>),
    Gc(Vec<GcEvent>),
    Query(Query, oneshot::Sender<Result<Reply>>),
    /// Stream the whole timeline to a chunked `.glr`; replies with on-disk size.
    Flush {
        path: PathBuf,
        pid: i32,
        epoch_ms: Option<u64>,
        reply: Sender<Result<u64>>,
    },
}

// ── Handle ──────────────────────────────────────────────────────────────────

/// Cheap, clonable handle to the DB thread. Counters (total/bytes/span/origin/
/// epoch) are kept here as atomics so `meta`/`head` need no query round-trip.
#[derive(Clone)]
pub struct Db {
    tx: Sender<Cmd>,
    total: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    span: Arc<AtomicU64>,
    origin: Arc<AtomicU64>,
    epoch: Arc<AtomicU64>, // 0 = unknown
    /// Live retention horizon (ns): oldest retained start when a cap is active,
    /// else 0. See [`Timeline::retained_from`].
    retained_from: Arc<AtomicU64>,
}

impl Db {
    /// Spawn the DB thread and return a handle. `cap_rows` bounds the in-memory
    /// timeline (oldest rows evicted past it) — use it for ephemeral live-view-only
    /// sessions; pass `None` when recording or opening a file so data is complete.
    pub fn spawn(cap_rows: Option<usize>) -> Result<Db> {
        let (tx, rx) = channel::<Cmd>();
        let retained_from = Arc::new(AtomicU64::new(0));
        let retained_for_thread = retained_from.clone();
        std::thread::Builder::new()
            .name("greenlane-db".into())
            .spawn(move || db_thread(rx, cap_rows, retained_for_thread))
            .context("spawning DB thread")?;
        Ok(Db {
            tx,
            total: Arc::new(AtomicU64::new(0)),
            bytes: Arc::new(AtomicU64::new(0)),
            span: Arc::new(AtomicU64::new(0)),
            origin: Arc::new(AtomicU64::new(ORIGIN_UNSET)),
            epoch: Arc::new(AtomicU64::new(0)),
            retained_from,
        })
    }

    /// Ingest a batch of executions (updates counters, then hands off to the thread).
    pub fn ingest_executions(&self, executions: Vec<Execution>) {
        if executions.is_empty() {
            return;
        }
        self.total
            .fetch_add(executions.len() as u64, Ordering::Relaxed);
        // `origin` is the FIRST observed start, set once on the first batch — not a
        // running minimum. Under out-of-order multi-thread arrival a later batch can
        // carry an earlier start, so `span_ns` (span − origin) may overstate slightly;
        // acceptable for a display origin, and the common single-thread case is exact.
        let _ = self.origin.compare_exchange(
            ORIGIN_UNSET,
            executions[0].start,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        let mut max_end = 0u64;
        for s in &executions {
            max_end = max_end.max(s.start.saturating_add(s.dur));
        }
        self.span.fetch_max(max_end, Ordering::Relaxed);
        // Per-batch and very hot (hundreds/s); trace, not debug. The periodic
        // flush below logs at debug for a coarser, readable cadence.
        trace!(
            batch = executions.len(),
            total = self.total.load(Ordering::Relaxed),
            span_ms = max_end / 1_000_000,
            "ingest executions"
        );
        let _ = self.tx.send(Cmd::Executions(executions));
    }

    pub fn ingest_gc(&self, gc: Vec<GcEvent>) {
        if gc.is_empty() {
            return;
        }
        debug!(batch = gc.len(), "ingest gc");
        let _ = self.tx.send(Cmd::Gc(gc));
    }

    /// Add to the raw event-stream byte counter (reported in the viewer header).
    pub fn add_bytes(&self, n: usize) {
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn set_epoch(&self, ms: u64) {
        self.epoch.store(ms, Ordering::Relaxed);
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    pub fn span(&self) -> u64 {
        self.span.load(Ordering::Relaxed)
    }
    pub fn origin(&self) -> u64 {
        match self.origin.load(Ordering::Relaxed) {
            ORIGIN_UNSET => 0,
            v => v,
        }
    }
    pub fn epoch(&self) -> Option<u64> {
        match self.epoch.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }
    /// Live retention horizon (ns): oldest start still retained when a cap is
    /// active, else 0 (nothing evicted / unbounded session).
    pub fn retained_from(&self) -> u64 {
        self.retained_from.load(Ordering::Relaxed)
    }

    /// Run a read query on the DB thread.
    pub async fn query(&self, q: Query) -> Result<Reply> {
        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(Cmd::Query(q, rtx))
            .map_err(|_| anyhow!("DB thread is gone"))?;
        rrx.await
            .map_err(|_| anyhow!("DB thread dropped the reply"))?
    }

    /// Synchronously write the whole timeline to `path` as a chunked `.glr`,
    /// streaming on the DB thread; returns the on-disk size. For the recorder.
    pub fn flush_to_file(&self, path: &Path, pid: i32) -> Result<u64> {
        let (reply, rx) = channel();
        self.tx
            .send(Cmd::Flush {
                path: path.to_path_buf(),
                pid,
                epoch_ms: self.epoch(),
                reply,
            })
            .map_err(|_| anyhow!("DB thread is gone"))?;
        rx.recv()
            .map_err(|_| anyhow!("DB thread dropped the reply"))?
    }
}

// ── DB thread ─────────────────────────────────────────────────────────────

/// Timeline state: the columnar DataFrame, GC events, and a mirror of execution
/// `start`s (aligned 1:1 with rows) for O(log n) viewport range lookups.
struct Timeline {
    df: Option<DataFrame>,
    gc: Vec<GcEvent>,
    /// `start` (ns) of every row in `df`, in row (ingest) order, 1:1 with rows.
    starts: Vec<i64>,
    /// True while `starts` is non-decreasing. A single cooperative runtime thread
    /// closes executions in start order so this holds (and executions never overlap), which
    /// lets [`window`] binary-search a contiguous row range. It is cleared the
    /// moment a new execution would break that: a `start` earlier than `max_end` means
    /// the execution either arrived out of order OR overlaps an earlier one (two runtime
    /// threads running at once) — both defeat the contiguous-range assumption, even
    /// when the starts themselves stay monotonic. After that we fall back to a full
    /// overlap scan, which is correct regardless of order/overlap.
    sorted: bool,
    /// Max execution END (start+dur, ns) appended so far. A subsequent `start` below
    /// this signals out-of-order arrival or overlap → clears `sorted`.
    max_end: i64,
    /// Evict oldest rows past this (live-view-only); `None` keeps everything.
    cap_rows: Option<usize>,
    /// Oldest retained execution start (ns) when a cap is active — the live retention
    /// horizon. `0` when nothing is capped. Surfaced so the viewer can show that
    /// data before this point has been evicted. Shared with the [`Db`] handle.
    retained_from: Arc<AtomicU64>,
}

/// A unit of off-thread work (a read query or a file flush).
type Job = Box<dyn FnOnce() + Send + 'static>;

/// Spawn `n` worker threads draining a shared job queue, returning a sender for
/// jobs. Concurrency is bounded by `n` regardless of how fast jobs arrive — the
/// queue absorbs bursts rather than each one spawning its own OS thread.
fn spawn_read_pool(n: usize) -> Sender<Job> {
    let (tx, rx) = channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..n {
        let rx = rx.clone();
        std::thread::Builder::new()
            .name("greenlane-db-read".into())
            .spawn(move || {
                loop {
                    // Hold the lock only to dequeue; run the job unlocked so workers
                    // execute in parallel.
                    let job = rx.lock().unwrap().recv();
                    match job {
                        Ok(job) => job(),
                        Err(_) => break, // sender dropped (DB thread exiting)
                    }
                }
            })
            .expect("spawning DB read worker");
    }
    tx
}

fn db_thread(rx: Receiver<Cmd>, cap_rows: Option<usize>, retained_from: Arc<AtomicU64>) {
    let mut tl = Timeline {
        df: None,
        gc: Vec::new(),
        starts: Vec::new(),
        sorted: true,
        max_end: i64::MIN,
        cap_rows,
        retained_from,
    };
    let mut pending: Vec<Execution> = Vec::new();
    // Bounded pool for the O(total) reads we run off the ingest thread.
    let pool = spawn_read_pool(READ_WORKERS);
    // Recording state (R7): a persistent append-only writer plus how far it has
    // sealed. Each Flush seals only the new rows/GC since the last one as a fresh
    // compressed segment, so we never re-encode the whole timeline.
    let mut rec: Option<crate::record::SegmentWriter> = None;
    let mut rec_rows = 0usize; // executions already sealed
    let mut rec_gc = 0usize; // GC events already sealed

    loop {
        match rx.recv_timeout(FLUSH_IDLE) {
            Ok(Cmd::Executions(mut v)) => {
                pending.append(&mut v);
                if pending.len() >= FLUSH_ROWS {
                    flush_pending(&mut tl, &mut pending);
                }
            }
            Ok(Cmd::Gc(mut v)) => tl.gc.append(&mut v),
            Ok(Cmd::Query(q, reply)) => {
                flush_pending(&mut tl, &mut pending);
                match q {
                    // Window is O(window) via the start index — run inline (it needs
                    // `starts`, which is large to clone).
                    Query::Window { t0, t1, cap } => {
                        let _ = reply.send(window(&tl, t0, t1, cap));
                    }
                    // Tail (live-follow append) is O(new rows) via the start index,
                    // like Window — run it inline too.
                    Query::Tail {
                        from,
                        gc_from,
                        t1,
                        cap,
                    } => {
                        let _ = reply.send(tail(&tl, from, gc_from, t1, cap));
                    }
                    // Summary reads both the DataFrame and the GC list (which lives
                    // on the timeline, not in `df`), so it runs inline like Window.
                    // `analyze` is a one-shot with no concurrent ingest, so the O(n)
                    // aggregation blocking this thread is fine.
                    Query::Summary {
                        warn_ns,
                        red_ns,
                        top,
                    } => {
                        let _ = reply.send(summary(&tl, warn_ns, red_ns, top));
                    }
                    // Detail runs inline too: with the `starts` index it narrows to a
                    // tiny contiguous row range (sorted case) before filtering, so it's
                    // cheap and needs `tl`, not a full-frame clone on the pool.
                    Query::Detail {
                        gid,
                        start_ns,
                        dur_ns,
                    } => {
                        let _ = reply.send(detail(&tl, gid, start_ns, dur_ns));
                    }
                    // Slowlog/Stats are O(total) Polars scans — run them OFF the
                    // ingest thread on a cheap (Arc) DataFrame snapshot, via the
                    // bounded pool so they don't stall ingestion or spawn unbounded
                    // threads under many viewers / rapid polling.
                    other => {
                        let df = tl.df.clone();
                        let _ = pool.send(Box::new(move || {
                            let _ = reply.send(run_read(df.as_ref(), other));
                        }));
                    }
                }
            }
            Ok(Cmd::Flush {
                path,
                pid,
                epoch_ms,
                reply,
            }) => {
                flush_pending(&mut tl, &mut pending);
                // Seal only the delta since the last flush as a new compressed
                // segment (R7). Inline on this thread: the delta is bounded by the
                // flush interval (not the whole timeline), so it can't grow into the
                // unbounded stall the old full-rewrite risked. Flushes are
                // synchronous (one in flight), so segment order is preserved.
                let _ = reply.send(seal_recording(
                    &mut rec,
                    &mut rec_rows,
                    &mut rec_gc,
                    &tl,
                    &path,
                    pid,
                    epoch_ms,
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                flush_pending(&mut tl, &mut pending);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
}

/// Append the pending executions to the timeline as a new chunk; keep `starts` in
/// sync, evict past the cap, and rechunk when fragmented.
fn flush_pending(tl: &mut Timeline, pending: &mut Vec<Execution>) {
    if pending.is_empty() {
        return;
    }
    let n = pending.len();
    match build_df(pending) {
        Ok(batch) => {
            // Apply to the DataFrame FIRST; only if that succeeds do we extend the
            // `starts` mirror and the sorted/max_end tracking. Doing it in this order
            // keeps the 1:1 `starts`↔rows invariant that `window_contiguous`'s binary
            // search relies on — a `vstack_mut` failure would otherwise leave `starts`
            // longer than `df`, silently corrupting every later window lookup.
            let applied = match &mut tl.df {
                Some(existing) => match existing.vstack_mut(&batch) {
                    Ok(_) => true,
                    Err(e) => {
                        error!(error = %e, "Polars vstack failed; dropping batch");
                        false
                    }
                },
                None => {
                    tl.df = Some(batch);
                    true
                }
            };
            if applied {
                // Append starts in row order and clear `sorted` if any execution starts
                // before the max END seen so far — out-of-order arrival OR an overlap
                // with an earlier execution (concurrent runtime threads). Either way the
                // contiguous-range window optimization no longer holds. (Checking
                // against max_end, not max_start, catches sorted-but-overlapping executions.)
                for s in pending.iter() {
                    let st = s.start as i64;
                    if st < tl.max_end {
                        tl.sorted = false;
                    }
                    tl.max_end = tl.max_end.max(st + s.dur as i64);
                    tl.starts.push(st);
                }
            }
        }
        Err(e) => error!(error = %format!("{e:#}"), "building Polars batch failed"),
    }
    pending.clear();

    // Evict oldest rows past the cap (live-view-only). Eviction drops the oldest
    // *ingested* rows (front of `starts`/`df`); `starts` stays aligned 1:1.
    if let (Some(df), Some(cap)) = (&mut tl.df, tl.cap_rows) {
        let h = df.height();
        if h > cap {
            let drop = h - cap;
            *df = df.slice(drop as i64, cap);
            tl.starts.drain(0..drop);
            debug!(evicted = drop, kept = cap, "evicted oldest rows (live cap)");
        }
        // Publish the retention horizon (oldest start still held), so the viewer
        // can show data before it was evicted. When sorted, that's the front row;
        // otherwise scan for the min (rare path — only with a cap + multi-thread).
        let horizon = if tl.sorted {
            tl.starts.first().copied()
        } else {
            tl.starts.iter().min().copied()
        };
        if let Some(v) = horizon {
            tl.retained_from.store(v as u64, Ordering::Relaxed);
        }
    }
    trace!(
        rows = n,
        df_height = tl.df.as_ref().map(|d| d.height()).unwrap_or(0),
        "flushed pending executions to DataFrame"
    );
}

/// Build a DataFrame from a batch of executions (computes is_hub once, here).
fn build_df(executions: &[Execution]) -> Result<DataFrame> {
    let n = executions.len();
    let (mut start, mut dur, mut gid) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    let (mut name, mut func, mut task, mut stack) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    let mut is_hub = Vec::with_capacity(n);
    for s in executions {
        start.push(s.start as i64);
        dur.push(s.dur as i64);
        gid.push(s.gid as i64);
        name.push(s.name.clone());
        func.push(s.func.clone());
        task.push(s.task.clone());
        stack.push(s.stack.clone());
        is_hub.push(crate::is_hub(&s.name));
    }
    df!(
        "start" => start, "dur" => dur, "gid" => gid,
        "name" => name, "func" => func, "task" => task, "stack" => stack,
        "is_hub" => is_hub,
    )
    .context("building timeline DataFrame")
}

// ── Queries ─────────────────────────────────────────────────────────────────

/// Off-thread reads (slowlog/stats) over a DataFrame snapshot. Window is handled
/// inline by the DB thread (it needs the start index).
fn run_read(df: Option<&DataFrame>, q: Query) -> Result<Reply> {
    match q {
        Query::Slowlog {
            warn_ns,
            red_ns,
            tier,
            sort_dur,
            limit,
        } => slowlog(df, warn_ns, red_ns, tier, sort_dur, limit),
        Query::Stats { t0, t1 } => stats(df, t0, t1),
        Query::Detail { .. } => unreachable!("detail runs inline on the DB thread"),
        Query::Window { .. } => unreachable!("window runs inline on the DB thread"),
        Query::Tail { .. } => unreachable!("tail runs inline on the DB thread"),
        Query::Summary { .. } => unreachable!("summary runs inline on the DB thread"),
    }
}

/// Look up one execution's func/task/stack by greenlet + start. The viewer's start is f32
/// ms (lossy), so match the row of `gid` whose start is nearest `start_ns`, within
/// `±max(dur_ns, 2ms)`. Cheap one-off (hover/click), not on the render path.
///
/// When the timeline is `sorted` (the common single-thread case), the candidate rows
/// — those whose start falls in `[lo, hi]` — form a contiguous range we binary-search
/// in the `starts` index, so we filter a tiny slice instead of scanning the whole
/// frame. The unsorted (overlapping multi-thread) case falls back to a full filter.
fn detail(tl: &Timeline, gid: u64, start_ns: u64, dur_ns: u64) -> Result<Reply> {
    let empty = || Reply::Detail {
        func: String::new(),
        task: String::new(),
        stack: String::new(),
    };
    let Some(df) = tl.df.as_ref() else {
        return Ok(empty());
    };
    let eps = dur_ns.max(2_000_000) as i64;
    let lo = (start_ns as i64).saturating_sub(eps);
    let hi = (start_ns as i64).saturating_add(eps);
    let gid_pred = col("gid").eq(lit(gid as i64));
    let out = if tl.sorted {
        // Contiguous candidate range by start, then filter that small slice by gid.
        let begin = tl.starts.partition_point(|&v| v < lo);
        let end = tl.starts.partition_point(|&v| v <= hi);
        if end <= begin {
            return Ok(empty());
        }
        df.slice(begin as i64, end - begin)
            .lazy()
            .filter(gid_pred)
            .select([col("start"), col("func"), col("task"), col("stack")])
            .collect()
            .context("detail query")?
    } else {
        df.clone()
            .lazy()
            .filter(
                gid_pred
                    .and(col("start").gt_eq(lit(lo)))
                    .and(col("start").lt_eq(lit(hi))),
            )
            .select([col("start"), col("func"), col("task"), col("stack")])
            .collect()
            .context("detail query")?
    };
    let h = out.height();
    if h == 0 {
        return Ok(empty());
    }
    let start = out.column("start")?.i64()?;
    let func = out.column("func")?.str()?;
    let task = out.column("task")?.str()?;
    let stack = out.column("stack")?.str()?;
    // Pick the row whose start is closest to the requested ns.
    let target = start_ns as i64;
    let mut best = 0usize;
    let mut best_d = i64::MAX;
    for i in 0..h {
        let d = (start.get(i).unwrap_or(0) - target).abs();
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    Ok(Reply::Detail {
        func: func.get(best).unwrap_or("").to_string(),
        task: task.get(best).unwrap_or("").to_string(),
        stack: stack.get(best).unwrap_or("").to_string(),
    })
}

/// Predicate: execution overlaps `[t0, t1]` (start < t1 AND start+dur > t0).
/// Times are clamped to i64 (the column type) so a u64::MAX sentinel can't wrap
/// negative and exclude everything.
fn overlaps(t0: u64, t1: u64) -> Expr {
    let cap = i64::MAX as u64;
    col("start")
        .lt(lit(t1.min(cap) as i64))
        .and((col("start") + col("dur")).gt(lit(t0.min(cap) as i64)))
}

/// Extract the render-only columns (`start`/`dur` ns, `gid`) for a window frame —
/// no `String` columns, so a wide window costs three numeric copies per row rather
/// than four heap allocations. func/task/stack are fetched lazily via [`detail`].
fn extract_window_cols(df: &DataFrame) -> Result<(Vec<i64>, Vec<i64>, Vec<u64>)> {
    let h = df.height();
    let start = df.column("start")?.i64()?;
    let dur = df.column("dur")?.i64()?;
    let gid = df.column("gid")?.i64()?;
    let mut starts = Vec::with_capacity(h);
    let mut durs = Vec::with_capacity(h);
    let mut gids = Vec::with_capacity(h);
    for i in 0..h {
        starts.push(start.get(i).unwrap_or(0));
        durs.push(dur.get(i).unwrap_or(0));
        gids.push(gid.get(i).unwrap_or(0) as u64);
    }
    Ok((starts, durs, gids))
}

/// Materialize full [`Execution`]s (with func/task/stack) from a frame — used only
/// where the strings are actually needed (sealing a recording segment), not on the
/// render path.
fn extract_executions(df: &DataFrame) -> Result<Vec<Execution>> {
    let h = df.height();
    let start = df.column("start")?.i64()?;
    let dur = df.column("dur")?.i64()?;
    let gid = df.column("gid")?.i64()?;
    let name = df.column("name")?.str()?;
    let func = df.column("func")?.str()?;
    let task = df.column("task")?.str()?;
    let stack = df.column("stack")?.str()?;
    let mut out = Vec::with_capacity(h);
    for i in 0..h {
        out.push(Execution {
            start: start.get(i).unwrap_or(0) as u64,
            dur: dur.get(i).unwrap_or(0) as u64,
            gid: gid.get(i).unwrap_or(0) as u64,
            name: name.get(i).unwrap_or("").to_string(),
            func: func.get(i).unwrap_or("").to_string(),
            task: task.get(i).unwrap_or("").to_string(),
            stack: stack.get(i).unwrap_or("").to_string(),
        });
    }
    Ok(out)
}

fn window(tl: &Timeline, t0: u64, t1: u64, cap: usize) -> Result<Reply> {
    let (sub, capped) = if tl.sorted {
        window_contiguous(tl, t0, t1, cap)?
    } else {
        window_scan(tl, t0, t1, cap)?
    };

    let (start, dur, gid) = match &sub {
        Some(df) => extract_window_cols(df)?,
        None => (Vec::new(), Vec::new(), Vec::new()),
    };
    let tracks = match &sub {
        Some(df) => track_runs(df)?,
        None => Vec::new(),
    };
    let gc_win = tl
        .gc
        .iter()
        .filter(|g| g.start < t1 && g.start.saturating_add(g.dur) > t0)
        .cloned()
        .collect();
    let visible = start.len();
    // Actual ns bounds of what we're returning (computed over the returned rows —
    // correct for both the sorted and the overlap-scan path).
    let min_start = start.iter().copied().min().unwrap_or(0).max(0) as u64;
    let max_start = start.iter().copied().max().unwrap_or(0).max(0) as u64;
    let max_end = start
        .iter()
        .zip(dur.iter())
        .map(|(s, d)| s.saturating_add(*d).max(0) as u64)
        .max()
        .unwrap_or(0);
    Ok(Reply::Window {
        start,
        dur,
        gid,
        tracks,
        gc: gc_win,
        visible,
        capped,
        sorted: tl.sorted,
        min_start,
        max_start,
        max_end,
    })
}

/// The new tail (live-follow append): rows whose START is in `(from, t1)`, where
/// `from` is the viewer's data frontier (max start it holds). Reuses the same compact
/// extraction + track rollup as [`window`], so the reply is a [`Reply::Window`] the
/// viewer appends rather than replaces. `gc_from` is the GC frontier (GC is sparse and
/// lags rows, so it gets its own cursor — using the row frontier would skip pauses).
fn tail(tl: &Timeline, from: u64, gc_from: u64, t1: u64, cap: usize) -> Result<Reply> {
    let (capped, sub) = tail_frame(tl, from, t1, cap)?;

    let (start, dur, gid) = match &sub {
        Some(df) => extract_window_cols(df)?,
        None => (Vec::new(), Vec::new(), Vec::new()),
    };
    let tracks = match &sub {
        Some(df) => track_runs(df)?,
        None => Vec::new(),
    };
    // New GC since the viewer's GC frontier: start > gc_from (strictly, so a pause it
    // already holds isn't re-sent) and start < t1.
    let gc_win = tl
        .gc
        .iter()
        .filter(|g| g.start > gc_from && g.start < t1)
        .cloned()
        .collect();
    let visible = start.len();
    let min_start = start.iter().copied().min().unwrap_or(0).max(0) as u64;
    let max_start = start.iter().copied().max().unwrap_or(0).max(0) as u64;
    let max_end = start
        .iter()
        .zip(dur.iter())
        .map(|(s, d)| s.saturating_add(*d).max(0) as u64)
        .max()
        .unwrap_or(0);
    Ok(Reply::Window {
        start,
        dur,
        gid,
        tracks,
        gc: gc_win,
        visible,
        capped,
        sorted: tl.sorted,
        min_start,
        max_start,
        max_end,
    })
}

/// Sub-frame of rows with START in `(from, t1)`, capped to `cap` (oldest-first, so a
/// truncated tail still abuts what the viewer already holds). Mirrors the two window
/// paths: a binary-searched contiguous slice when `sorted`, else a bounded scan.
fn tail_frame(tl: &Timeline, from: u64, t1: u64, cap: usize) -> Result<(bool, Option<DataFrame>)> {
    let Some(df) = tl.df.as_ref() else {
        return Ok((false, None));
    };
    if tl.sorted {
        // start > from .. start < t1, contiguous in the ascending start index. The
        // lower bound is EXCLUSIVE — `from` is the max start the viewer already holds,
        // so only strictly-later rows are new (no dup); the upper bound is exclusive
        // to match `window`'s `start < t1`.
        let begin = tl.starts.partition_point(|&v| (v as u64) <= from);
        let end = tl.starts.partition_point(|&v| (v as u64) < t1);
        let n = end.saturating_sub(begin);
        if n == 0 {
            return Ok((false, None));
        }
        let capped = n > cap;
        let len = if capped { cap } else { n };
        Ok((capped, Some(df.slice(begin as i64, len))))
    } else {
        let cap_i = i64::MAX as u64;
        let pred = col("start")
            .gt(lit(from.min(cap_i) as i64))
            .and(col("start").lt(lit(t1.min(cap_i) as i64)));
        let out = df
            .clone()
            .lazy()
            .filter(pred)
            .limit((cap as u32).saturating_add(1))
            .collect()
            .context("tail scan")?;
        let capped = out.height() > cap;
        let sub = if capped { out.slice(0, cap) } else { out };
        Ok((capped, Some(sub)))
    }
}

/// Fast path (single cooperative thread): executions don't overlap and `starts` is
/// ascending, so the rows overlapping `[t0,t1]` are a CONTIGUOUS range — from the
/// execution straddling t0 up to (not incl.) the first execution starting at/after t1.
/// Binary-search it and slice it out, no full scan. Returns the sub-frame (or `None`
/// when empty) plus whether the range was truncated to `cap`.
fn window_contiguous(
    tl: &Timeline,
    t0: u64,
    t1: u64,
    cap: usize,
) -> Result<(Option<DataFrame>, bool)> {
    let hi = tl.starts.partition_point(|&v| (v as u64) < t1);
    let lo = tl
        .starts
        .partition_point(|&v| (v as u64) <= t0)
        .saturating_sub(1)
        .min(hi);
    let in_range = hi - lo;
    let capped = in_range > cap;
    // When over the cap, keep the CENTER of the range rather than the first rows:
    // the client fetches a margin around the visible interval, so the center is
    // what's actually on screen. (True per-pixel LOD downsampling is the next step.)
    let (start_idx, len) = if capped {
        (lo + (in_range - cap) / 2, cap)
    } else {
        (lo, in_range)
    };
    let sub = match (&tl.df, len) {
        (Some(df), n) if n > 0 => Some(df.slice(start_idx as i64, n)),
        _ => None,
    };
    Ok((sub, capped))
}

/// Correct path for multi-thread captures: per-thread executions can overlap and
/// arrive out of start order, so the rows overlapping `[t0,t1]` are NOT a
/// contiguous range. Filter the whole DataFrame by overlap (a Polars scan). Over
/// the cap, take the first `cap` matches (a per-pixel LOD downsample is the next
/// step); `capped` flags the truncation.
fn window_scan(tl: &Timeline, t0: u64, t1: u64, cap: usize) -> Result<(Option<DataFrame>, bool)> {
    let df = match &tl.df {
        Some(d) => d,
        None => return Ok((None, false)),
    };
    // Bound the scan: `limit(cap+1)` stops Polars after at most cap+1 matches, so
    // `collect()` never materializes the full overlapping set — WINDOW_CAP caps
    // server memory/CPU even on dense/overlapping captures. The +1 is just enough
    // to detect (and flag) truncation.
    let out = df
        .clone()
        .lazy()
        .filter(overlaps(t0, t1))
        .limit((cap as u32).saturating_add(1))
        .collect()
        .context("window overlap scan")?;
    let capped = out.height() > cap;
    let sub = if capped { out.slice(0, cap) } else { out };
    Ok((Some(sub), capped))
}

/// Sum run-time per track over the windowed sub-frame (for activity sort + labels),
/// via a Polars group-by on `gid` so per-row names are never materialized — only
/// one name per distinct greenlet in the window.
fn track_runs(df: &DataFrame) -> Result<Vec<TrackRun>> {
    let g = df
        .clone()
        .lazy()
        .group_by([col("gid")])
        .agg([
            col("dur").sum().alias("run"),
            col("name").first().alias("name"),
            col("is_hub").first().alias("is_hub"),
        ])
        .collect()
        .context("window track rollup")?;
    let gid = g.column("gid")?.i64()?;
    let run = g.column("run")?.i64()?;
    let name = g.column("name")?.str()?;
    let is_hub = g.column("is_hub")?.bool()?;
    Ok((0..g.height())
        .map(|i| TrackRun {
            gid: gid.get(i).unwrap_or(0) as u64,
            name: name.get(i).unwrap_or("").to_string(),
            is_hub: is_hub.get(i).unwrap_or(false),
            run_ns: run.get(i).unwrap_or(0).max(0) as u64,
        })
        .collect())
}

fn slowlog(
    df: Option<&DataFrame>,
    warn_ns: u64,
    red_ns: u64,
    tier: SlowTier,
    sort_dur: bool,
    limit: usize,
) -> Result<Reply> {
    let df = match df {
        None => {
            return Ok(Reply::Slowlog {
                rows: Vec::new(),
                total: 0,
            });
        }
        Some(df) => df,
    };
    // Tier predicate on dur — selected here (not after a limited page) so a tier's
    // count + rows are correct even when another tier dominates the newest executions.
    let dur = col("dur");
    let tier_pred = match tier {
        SlowTier::All => dur.clone().gt_eq(lit(warn_ns as i64)),
        SlowTier::Warn => dur
            .clone()
            .gt_eq(lit(warn_ns as i64))
            .and(dur.clone().lt(lit(red_ns as i64))),
        SlowTier::Block => dur.clone().gt_eq(lit(red_ns as i64)),
    };
    // Filter once; reuse the lazy plan for both the count and the page so the
    // sort + limit run inside Polars rather than materializing every match and
    // sorting/truncating in Rust.
    let filtered = df.clone().lazy().filter(col("is_hub").not().and(tier_pred));

    // `total` is the real number of matching slow executions (drives the badge);
    // computed as an aggregation so it doesn't materialize the rows.
    let count = filtered
        .clone()
        .select([len().cast(DataType::Int64).alias("n")])
        .collect()
        .context("slowlog count")?;
    let total = get_i64(&count, "n").max(0) as usize;

    // Sort + limit pushed into Polars: longest-first by dur, else newest-first by
    // start. Both are descending; `limit` bounds the rows shipped for display.
    let sort_col = if sort_dur { "dur" } else { "start" };
    let out = filtered
        .sort(
            [sort_col],
            SortMultipleOptions::default().with_order_descending(true),
        )
        .limit(limit as u32)
        .select([
            col("start"),
            col("dur"),
            col("gid"),
            col("name"),
            col("func"),
        ])
        .collect()
        .context("slowlog query")?;

    let h = out.height();
    let start = out.column("start")?.i64()?;
    let dur = out.column("dur")?.i64()?;
    let gid = out.column("gid")?.i64()?;
    let name = out.column("name")?.str()?;
    let func = out.column("func")?.str()?;
    let rows: Vec<SlowRow> = (0..h)
        .map(|i| {
            let d = dur.get(i).unwrap_or(0) as u64;
            SlowRow {
                start: start.get(i).unwrap_or(0) as u64,
                dur: d,
                gid: gid.get(i).unwrap_or(0) as u64,
                name: name.get(i).unwrap_or("").to_string(),
                func: func.get(i).unwrap_or("").to_string(),
                level: if d >= red_ns { 2 } else { 1 },
            }
        })
        .collect();
    Ok(Reply::Slowlog { rows, total })
}

fn stats(df: Option<&DataFrame>, t0: u64, t1: u64) -> Result<Reply> {
    let (p50, p95, p99) = match df {
        None => (0.0, 0.0, 0.0),
        Some(df) => {
            let out = df
                .clone()
                .lazy()
                .filter(col("is_hub").not().and(overlaps(t0, t1)))
                .select([
                    col("dur")
                        .quantile(lit(0.5), QuantileMethod::Linear)
                        .alias("p50"),
                    col("dur")
                        .quantile(lit(0.95), QuantileMethod::Linear)
                        .alias("p95"),
                    col("dur")
                        .quantile(lit(0.99), QuantileMethod::Linear)
                        .alias("p99"),
                ])
                .collect()
                .context("stats query")?;
            (
                get_f64(&out, "p50"),
                get_f64(&out, "p95"),
                get_f64(&out, "p99"),
            )
        }
    };
    Ok(Reply::Stats { p50, p95, p99 })
}

/// Whole-capture aggregates for `analyze`. Runs inline on the DB thread because it
/// needs `tl.gc` (GC lives on the timeline, not the DataFrame) alongside the
/// DataFrame scans for greenlet/function rollups.
fn summary(tl: &Timeline, warn_ns: u64, red_ns: u64, top: usize) -> Result<Reply> {
    use std::collections::BTreeMap;

    // GC aggregates straight from the timeline's GC list.
    let gc_count = tl.gc.len() as u64;
    let mut gc_total_ns = 0u64;
    let mut by_gen: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    for g in &tl.gc {
        gc_total_ns = gc_total_ns.saturating_add(g.dur);
        let e = by_gen.entry(g.generation).or_insert((0, 0));
        e.0 += 1;
        e.1 = e.1.saturating_add(g.dur);
    }
    let gc_by_gen: Vec<GcGen> = by_gen
        .into_iter()
        .map(|(generation, (count, total_ns))| GcGen {
            generation,
            count,
            total_ns,
        })
        .collect();

    let df = match &tl.df {
        None => {
            return Ok(Reply::Summary {
                greenlets: 0,
                hub_run_ns: 0,
                nonhub_run_ns: 0,
                warn_count: 0,
                block_count: 0,
                top_greenlets: Vec::new(),
                top_funcs: Vec::new(),
                gc_count,
                gc_total_ns,
                gc_by_gen,
            });
        }
        Some(df) => df,
    };

    // Scalars in one pass: distinct greenlets, hub/non-hub run totals, slow counts.
    let nonhub = || col("is_hub").not();
    let agg = df
        .clone()
        .lazy()
        .select([
            col("gid")
                .n_unique()
                .cast(DataType::Int64)
                .alias("greenlets"),
            col("dur").filter(col("is_hub")).sum().alias("hub_run"),
            col("dur").filter(nonhub()).sum().alias("nonhub_run"),
            col("dur")
                .filter(nonhub().and(col("dur").gt_eq(lit(warn_ns as i64))))
                .count()
                .cast(DataType::Int64)
                .alias("warn_n"),
            col("dur")
                .filter(nonhub().and(col("dur").gt_eq(lit(red_ns as i64))))
                .count()
                .cast(DataType::Int64)
                .alias("block_n"),
        ])
        .collect()
        .context("summary scalars")?;
    let greenlets = get_i64(&agg, "greenlets").max(0) as u64;
    let hub_run_ns = get_i64(&agg, "hub_run").max(0) as u64;
    let nonhub_run_ns = get_i64(&agg, "nonhub_run").max(0) as u64;
    let warn_count = get_i64(&agg, "warn_n").max(0) as u64;
    let block_count = get_i64(&agg, "block_n").max(0) as u64;

    // Top greenlets by total run time (longest first).
    let greenlet_df = df
        .clone()
        .lazy()
        .group_by([col("gid")])
        .agg([
            col("dur").sum().alias("run"),
            col("name").first().alias("name"),
            col("is_hub").first().alias("is_hub"),
            col("dur").count().cast(DataType::Int64).alias("executions"),
        ])
        .sort(
            ["run"],
            SortMultipleOptions::default().with_order_descending(true),
        )
        .limit(top as u32)
        .collect()
        .context("summary top greenlets")?;
    let top_greenlets = {
        let gid = greenlet_df.column("gid")?.i64()?;
        let run = greenlet_df.column("run")?.i64()?;
        let name = greenlet_df.column("name")?.str()?;
        let is_hub = greenlet_df.column("is_hub")?.bool()?;
        let executions = greenlet_df.column("executions")?.i64()?;
        (0..greenlet_df.height())
            .map(|i| GreenletAgg {
                gid: gid.get(i).unwrap_or(0) as u64,
                name: name.get(i).unwrap_or("").to_string(),
                is_hub: is_hub.get(i).unwrap_or(false),
                run_ns: run.get(i).unwrap_or(0).max(0) as u64,
                executions: executions.get(i).unwrap_or(0).max(0) as u64,
            })
            .collect()
    };

    // Hottest non-Hub functions by total run time.
    let func_df = df
        .clone()
        .lazy()
        .filter(nonhub())
        .group_by([col("func")])
        .agg([
            col("dur").sum().alias("total"),
            col("dur").count().cast(DataType::Int64).alias("cnt"),
            col("dur").max().alias("mx"),
        ])
        .sort(
            ["total"],
            SortMultipleOptions::default().with_order_descending(true),
        )
        .limit(top as u32)
        .collect()
        .context("summary top funcs")?;
    let top_funcs = {
        let func = func_df.column("func")?.str()?;
        let total = func_df.column("total")?.i64()?;
        let cnt = func_df.column("cnt")?.i64()?;
        let mx = func_df.column("mx")?.i64()?;
        (0..func_df.height())
            .map(|i| FuncAgg {
                func: func.get(i).unwrap_or("").to_string(),
                total_ns: total.get(i).unwrap_or(0).max(0) as u64,
                count: cnt.get(i).unwrap_or(0).max(0) as u64,
                max_ns: mx.get(i).unwrap_or(0).max(0) as u64,
            })
            .collect()
    };

    Ok(Reply::Summary {
        greenlets,
        hub_run_ns,
        nonhub_run_ns,
        warn_count,
        block_count,
        top_greenlets,
        top_funcs,
        gc_count,
        gc_total_ns,
        gc_by_gen,
    })
}

fn get_f64(df: &DataFrame, col: &str) -> f64 {
    df.column(col)
        .ok()
        .and_then(|c| c.f64().ok().and_then(|c| c.get(0)))
        .unwrap_or(0.0)
}

fn get_i64(df: &DataFrame, col: &str) -> i64 {
    df.column(col)
        .ok()
        .and_then(|c| c.i64().ok().and_then(|c| c.get(0)))
        .unwrap_or(0)
}

// ── Sealed-segment recording (R7) ────────────────────────────────────────────

/// Seal the new rows + GC since the last flush as a fresh compressed `.glr`
/// segment, appended to the recording (created on first use). The delta is
/// extracted from the timeline's stable row order — recordings never evict
/// (`cap_rows = None`), so `rec_rows`/`rec_gc` stay valid offsets. Returns the
/// recording's on-disk size. Runs on the DB thread; flushes are serialized by the
/// synchronous caller, so segments are appended in order.
fn seal_recording(
    rec: &mut Option<crate::record::SegmentWriter>,
    rec_rows: &mut usize,
    rec_gc: &mut usize,
    tl: &Timeline,
    path: &Path,
    pid: i32,
    epoch_ms: Option<u64>,
) -> Result<u64> {
    if rec.is_none() {
        *rec = Some(crate::record::SegmentWriter::create(path)?);
    }
    let writer = rec.as_mut().unwrap();

    let height = tl.df.as_ref().map(|d| d.height()).unwrap_or(0);
    // Guard against the (recording-mode-impossible) case of eviction shrinking the
    // frame below our cursor.
    let lo = (*rec_rows).min(height);
    let new_executions: Vec<Execution> = match tl.df.as_ref() {
        Some(df) if height > lo => extract_executions(&df.slice(lo as i64, height - lo))?,
        _ => Vec::new(),
    };
    let glo = (*rec_gc).min(tl.gc.len());
    let new_gc = &tl.gc[glo..];

    writer.seal_segment(&new_executions, new_gc, pid, epoch_ms)?;
    *rec_rows = height;
    *rec_gc = tl.gc.len();

    let bytes = writer.size();
    debug!(
        new_executions = new_executions.len(),
        new_gc = new_gc.len(),
        total_bytes = bytes,
        path = %path.display(),
        "sealed .glr segment"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    //! Core data-layer tests: drive the real DB thread through its public API
    //! (ingest → query) and assert the window/slowlog/stats contracts the server
    //! relies on. Run with `cargo test`.
    use super::*;
    use crate::store::{Execution, GcEvent};

    const MS: u64 = 1_000_000; // ns per ms

    fn sl(gid: u64, start_ms: u64, dur_ms: u64, name: &str) -> Execution {
        Execution {
            gid,
            start: start_ms * MS,
            dur: dur_ms * MS,
            name: name.into(),
            func: format!("app.py:work_{gid}:1"),
            task: String::new(),
            stack: String::new(),
        }
    }

    #[tokio::test]
    async fn counters_track_origin_span_and_total() {
        let db = Db::spawn(None).unwrap();
        db.ingest_executions(vec![sl(1, 5, 10, "Greenlet-1"), sl(2, 20, 5, "Greenlet-2")]);
        assert_eq!(db.total(), 2);
        assert_eq!(db.origin(), 5 * MS); // first execution's start
        assert_eq!(db.span(), 25 * MS); // max(start+dur) = 20+5
        db.set_epoch(1700);
        assert_eq!(db.epoch(), Some(1700));
    }

    #[tokio::test]
    async fn window_handles_overlapping_out_of_order_executions() {
        // Multi-thread capture: a long execution on one thread (gid 1, [0,100]) closes
        // — and so ingests — AFTER a later-starting, shorter execution on another thread
        // (gid 2, [10,15]). Starts arrive non-monotonic (10 then 0), so the DB must
        // switch off the contiguous fast path and still return BOTH executions for a
        // window inside the overlap.
        let db = Db::spawn(None).unwrap();
        db.ingest_executions(vec![
            sl(2, 10, 5, "Loop-2"),  // [10,15] ingested first
            sl(1, 0, 100, "Loop-1"), // [0,100] starts earlier, ingested later
        ]);
        let r = db
            .query(Query::Window {
                t0: 12 * MS,
                t1: 14 * MS,
                cap: 100,
            })
            .await
            .unwrap();
        match r {
            Reply::Window { gid, .. } => {
                assert!(
                    gid.contains(&1),
                    "long overlapping execution must not be dropped"
                );
                assert!(gid.contains(&2), "short execution overlapping the window");
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn window_handles_sorted_but_overlapping_executions() {
        // Starts arrive MONOTONIC (0 then 3) yet the executions OVERLAP: a short execution
        // gid 1 [0,5] then a long gid 2 [3,103] (its later end ingested after). The
        // start-order check alone wouldn't notice, but tracking max_end does — the
        // DB must scan, not binary-search a contiguous range, or it would drop the
        // earlier long-lived execution for a window past its start.
        let db = Db::spawn(None).unwrap();
        db.ingest_executions(vec![
            sl(1, 0, 5, "A"),   // [0,5]
            sl(2, 3, 100, "B"), // [3,103] — starts after A but overlaps it
        ]);
        // Window [4,4.5] overlaps BOTH (A ends at 5, B spans it). With a contiguous
        // range keyed on start, A (row 0) would be skipped.
        let r = db
            .query(Query::Window {
                t0: 4 * MS,
                t1: 4 * MS + MS / 2,
                cap: 100,
            })
            .await
            .unwrap();
        match r {
            Reply::Window { gid, .. } => {
                assert!(
                    gid.contains(&1),
                    "earlier overlapping execution must not be dropped"
                );
                assert!(
                    gid.contains(&2),
                    "later long execution overlapping the window"
                );
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn window_returns_only_overlapping_executions() {
        let db = Db::spawn(None).unwrap();
        // Executions always arrive in ascending start order (the collector closes them
        // in switch order, and time only moves forward) — the window index relies
        // on that, so the test ingests them sorted by start.
        db.ingest_executions(vec![
            sl(1, 0, 10, "Greenlet-1"),  // [0,10]   ends before 20 → out
            sl(3, 18, 5, "Greenlet-3"),  // [18,23]  overlaps [20,40] → in
            sl(2, 50, 10, "Greenlet-2"), // [50,60]  starts after 40 → out
        ]);
        let r = db
            .query(Query::Window {
                t0: 20 * MS,
                t1: 40 * MS,
                cap: 100,
            })
            .await
            .unwrap();
        match r {
            Reply::Window {
                gid,
                visible,
                capped,
                ..
            } => {
                assert!(!capped);
                assert_eq!(visible, gid.len());
                assert!(gid.contains(&3));
                assert!(!gid.contains(&1)); // ended at 10, before 20
                assert!(!gid.contains(&2)); // starts at 50, after 40
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn window_cap_truncates_and_flags() {
        let db = Db::spawn(None).unwrap();
        db.ingest_executions((0..5).map(|i| sl(i, i * 2, 1, "Greenlet")).collect());
        let r = db
            .query(Query::Window {
                t0: 0,
                t1: u64::MAX >> 1,
                cap: 2,
            })
            .await
            .unwrap();
        match r {
            Reply::Window { gid, capped, .. } => {
                assert_eq!(gid.len(), 2);
                assert!(capped);
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn tail_returns_only_new_rows_past_the_frontier() {
        // A live-follow append: `from` is the viewer's data frontier — the max start
        // it already holds (here start 10, from rows 1 & 2). Tail returns rows with
        // START strictly > from (a start-bound test, NOT overlap), so the long span
        // straddling the frontier (gid 1, started at 0) is NOT re-sent, and the row
        // exactly at the frontier (gid 2, start 10) isn't duplicated — only the
        // genuinely new rows past it come back. `max_start` is the next frontier.
        let db = Db::spawn(None).unwrap();
        db.ingest_executions(vec![
            sl(1, 0, 100, "Greenlet-1"), // [0,100] started at/before frontier → excluded
            sl(2, 10, 5, "Greenlet-2"),  // start 10 == frontier → not re-sent
            sl(3, 30, 5, "Greenlet-3"),  // start 30 > 10 → included
            sl(4, 45, 5, "Greenlet-4"),  // start 45 → included
        ]);
        let r = db
            .query(Query::Tail {
                from: 10 * MS,
                gc_from: 0,
                t1: 100 * MS,
                cap: 100,
            })
            .await
            .unwrap();
        match r {
            Reply::Window {
                gid,
                capped,
                start,
                max_start,
                ..
            } => {
                assert!(!capped);
                assert_eq!(start.len(), 2);
                assert!(gid.contains(&3));
                assert!(gid.contains(&4));
                assert!(
                    !gid.contains(&1),
                    "span straddling the frontier is not re-sent"
                );
                assert!(
                    !gid.contains(&2),
                    "row at exactly the frontier is not duplicated"
                );
                assert_eq!(
                    max_start,
                    45 * MS,
                    "max_start is the viewer's next frontier"
                );
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn tail_only_includes_gc_past_the_gc_frontier() {
        let db = Db::spawn(None).unwrap();
        db.ingest_executions(vec![sl(1, 0, 100, "Greenlet-1")]);
        db.ingest_gc(vec![
            GcEvent {
                start: 5 * MS,
                dur: MS,
                generation: 0,
                collected: 1,
            }, // at the gc frontier → not re-sent
            GcEvent {
                start: 60 * MS,
                dur: MS,
                generation: 2,
                collected: 7,
            }, // past it → included
        ]);
        let r = db
            .query(Query::Tail {
                from: 0,
                gc_from: 5 * MS,
                t1: 100 * MS,
                cap: 100,
            })
            .await
            .unwrap();
        match r {
            Reply::Window { gc, .. } => {
                assert_eq!(gc.len(), 1);
                assert_eq!(gc[0].generation, 2);
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn slowlog_filters_by_threshold_and_tier() {
        let db = Db::spawn(None).unwrap();
        db.ingest_executions(vec![
            sl(1, 0, 5, "Greenlet-1"),   // below warn
            sl(2, 10, 25, "Greenlet-2"), // warn tier (>20ms, <50ms)
            sl(3, 40, 80, "Greenlet-3"), // block tier (>50ms)
            sl(4, 0, 90, "Hub"),         // long but Hub → excluded
        ]);
        let all = db
            .query(Query::Slowlog {
                warn_ns: 20 * MS,
                red_ns: 50 * MS,
                tier: SlowTier::All,
                sort_dur: true,
                limit: 100,
            })
            .await
            .unwrap();
        match all {
            Reply::Slowlog { rows, total } => {
                assert_eq!(total, 2); // gid 2 and 3, Hub excluded
                assert_eq!(rows[0].gid, 3); // sorted by dur desc
                assert_eq!(rows[0].level, 2); // block tier
            }
            _ => panic!("expected Slowlog reply"),
        }
        let block = db
            .query(Query::Slowlog {
                warn_ns: 20 * MS,
                red_ns: 50 * MS,
                tier: SlowTier::Block,
                sort_dur: false,
                limit: 100,
            })
            .await
            .unwrap();
        match block {
            Reply::Slowlog { rows, total } => {
                assert_eq!(total, 1); // only gid 3
                assert_eq!(rows[0].gid, 3);
            }
            _ => panic!("expected Slowlog reply"),
        }
        // The warn tier is its own band (warn <= dur < block): only gid 2, even
        // though gid 3 is also over the warn threshold.
        let warn = db
            .query(Query::Slowlog {
                warn_ns: 20 * MS,
                red_ns: 50 * MS,
                tier: SlowTier::Warn,
                sort_dur: false,
                limit: 100,
            })
            .await
            .unwrap();
        match warn {
            Reply::Slowlog { rows, total } => {
                assert_eq!(total, 1); // only gid 2
                assert_eq!(rows[0].gid, 2);
                assert_eq!(rows[0].level, 1); // warn tier
            }
            _ => panic!("expected Slowlog reply"),
        }
    }

    #[tokio::test]
    async fn stats_reports_percentiles_excluding_hub() {
        let db = Db::spawn(None).unwrap();
        let mut executions: Vec<Execution> = (1..=100).map(|i| sl(i, i, 10, "Greenlet")).collect();
        executions.push(sl(999, 0, 100_000, "Hub")); // huge, but Hub → must not skew
        db.ingest_executions(executions);
        let r = db
            .query(Query::Stats {
                t0: 0,
                t1: u64::MAX >> 1,
            })
            .await
            .unwrap();
        match r {
            Reply::Stats { p50, p95, p99 } => {
                // All non-Hub durations are 10ms, so every percentile ≈ 10ms.
                for p in [p50, p95, p99] {
                    assert!((p - 10.0 * MS as f64).abs() < MS as f64, "p={p}");
                }
            }
            _ => panic!("expected Stats reply"),
        }
    }

    #[tokio::test]
    async fn summary_aggregates_greenlets_funcs_and_gc() {
        let db = Db::spawn(None).unwrap();
        // Two app greenlets sharing a func, plus a long Hub execution (excluded from work
        // metrics) and a fast sub-warn execution. gid 2 is a block-tier stall.
        db.ingest_executions(vec![
            sl(1, 0, 25, "Greenlet-1"),  // warn tier (>=20ms, <50ms)
            sl(2, 30, 80, "Greenlet-2"), // block tier (>=50ms)
            sl(1, 200, 1, "Greenlet-1"), // fast — below warn
            sl(9, 0, 500, "Hub"),        // hub — excluded from non-hub run/counts
        ]);
        db.ingest_gc(vec![
            GcEvent {
                start: 5 * MS,
                dur: 2 * MS,
                generation: 0,
                collected: 1,
            },
            GcEvent {
                start: 40 * MS,
                dur: 3 * MS,
                generation: 2,
                collected: 9,
            },
        ]);
        let r = db
            .query(Query::Summary {
                warn_ns: 20 * MS,
                red_ns: 50 * MS,
                top: 10,
            })
            .await
            .unwrap();
        match r {
            Reply::Summary {
                greenlets,
                hub_run_ns,
                nonhub_run_ns,
                warn_count,
                block_count,
                top_greenlets,
                top_funcs,
                gc_count,
                gc_total_ns,
                gc_by_gen,
            } => {
                assert_eq!(greenlets, 3); // gid 1, 2, 9
                assert_eq!(hub_run_ns, 500 * MS);
                assert_eq!(nonhub_run_ns, (25 + 80 + 1) * MS); // hub excluded
                assert_eq!(warn_count, 2); // gid 1 (25ms) + gid 2 (80ms)
                assert_eq!(block_count, 1); // only gid 2
                // Top greenlet by run time is the Hub (500ms).
                assert_eq!(top_greenlets[0].gid, 9);
                assert!(top_greenlets[0].is_hub);
                // Hottest non-hub func is gid 2's (80ms); Hub's func is excluded.
                assert_eq!(top_funcs[0].func, "app.py:work_2:1");
                assert_eq!(top_funcs[0].total_ns, 80 * MS);
                assert_eq!(gc_count, 2);
                assert_eq!(gc_total_ns, 5 * MS);
                assert_eq!(gc_by_gen.len(), 2);
                assert_eq!(gc_by_gen[0].generation, 0); // BTreeMap → gen-ordered
                assert_eq!(gc_by_gen[0].total_ns, 2 * MS);
            }
            _ => panic!("expected Summary reply"),
        }
    }

    #[tokio::test]
    async fn window_includes_gc_in_range() {
        let db = Db::spawn(None).unwrap();
        db.ingest_executions(vec![sl(1, 0, 100, "Greenlet-1")]);
        db.ingest_gc(vec![
            GcEvent {
                start: 5 * MS,
                dur: MS,
                generation: 2,
                collected: 7,
            },
            GcEvent {
                start: 500 * MS,
                dur: MS,
                generation: 0,
                collected: 1,
            },
        ]);
        let r = db
            .query(Query::Window {
                t0: 0,
                t1: 50 * MS,
                cap: 100,
            })
            .await
            .unwrap();
        match r {
            Reply::Window { gc, .. } => {
                assert_eq!(gc.len(), 1);
                assert_eq!(gc[0].generation, 2);
            }
            _ => panic!("expected Window reply"),
        }
    }
}
