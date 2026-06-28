//! Embedded Polars-backed timeline store.
//!
//! All captured slices live in an in-memory Polars `DataFrame` (columnar Arrow,
//! pure Rust — no C++/system deps, links cleanly with musl), so neither the Rust
//! process nor the browser holds the timeline as a fat `Vec<Slice>`. A single
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

use crate::store::{GcEvent, Slice};

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
    /// Slices overlapping `[t0, t1]` ns, plus track/GC rollups for that window.
    Window {
        t0: u64,
        t1: u64,
        /// Hard cap on returned slices (memory bound); `capped` flags truncation.
        cap: usize,
    },
    /// Newest slow spans (non-Hub), filtered + ordered for the slow-log panel.
    Slowlog {
        warn_ns: u64,
        red_ns: u64,
        tier: SlowTier,
        sort_dur: bool,
        limit: usize,
    },
    /// Duration percentiles (non-Hub) over `[t0, t1]` (whole timeline if full range).
    Stats { t0: u64, t1: u64 },
}

/// Which slow-log tier to return — selected server-side so the display `limit`
/// never hides matching rows of the requested tier (filtering `warn` *after* a
/// limited page could miss warn spans when blocks dominate the newest rows).
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

/// The DB thread's reply to a [`Query`].
pub enum Reply {
    Window {
        slices: Vec<Slice>,
        tracks: Vec<TrackRun>,
        gc: Vec<GcEvent>,
        visible: usize,
        capped: bool,
        /// Absolute ns bounds of the slices actually returned (min start, max
        /// end). `0`/`0` when the window is empty. Lets the viewer record the range
        /// it truly has — when `capped` truncates an edge, the requested range
        /// overstates coverage.
        min_start: u64,
        max_end: u64,
    },
    Slowlog {
        rows: Vec<SlowRow>,
        /// Total matching slow spans (before the display limit) — for the badge.
        total: usize,
    },
    Stats {
        p50: f64,
        p95: f64,
        p99: f64,
    },
}

enum Cmd {
    Slices(Vec<Slice>),
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

    /// Ingest a batch of slices (updates counters, then hands off to the thread).
    pub fn ingest_slices(&self, slices: Vec<Slice>) {
        if slices.is_empty() {
            return;
        }
        self.total.fetch_add(slices.len() as u64, Ordering::Relaxed);
        let _ = self.origin.compare_exchange(
            ORIGIN_UNSET,
            slices[0].start,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        let mut max_end = 0u64;
        for s in &slices {
            max_end = max_end.max(s.start.saturating_add(s.dur));
        }
        self.span.fetch_max(max_end, Ordering::Relaxed);
        // Per-batch and very hot (hundreds/s); trace, not debug. The periodic
        // flush below logs at debug for a coarser, readable cadence.
        trace!(
            batch = slices.len(),
            total = self.total.load(Ordering::Relaxed),
            span_ms = max_end / 1_000_000,
            "ingest slices"
        );
        let _ = self.tx.send(Cmd::Slices(slices));
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

/// Timeline state: the columnar DataFrame, GC events, and a mirror of slice
/// `start`s (aligned 1:1 with rows) for O(log n) viewport range lookups.
struct Timeline {
    df: Option<DataFrame>,
    gc: Vec<GcEvent>,
    /// `start` (ns) of every row in `df`, in row (ingest) order, 1:1 with rows.
    starts: Vec<i64>,
    /// True while `starts` is non-decreasing. A single cooperative runtime thread
    /// closes spans in start order so this holds (and spans never overlap), which
    /// lets [`window`] binary-search a contiguous row range. It is cleared the
    /// moment a new span would break that: a `start` earlier than `max_end` means
    /// the span either arrived out of order OR overlaps an earlier one (two runtime
    /// threads running at once) — both defeat the contiguous-range assumption, even
    /// when the starts themselves stay monotonic. After that we fall back to a full
    /// overlap scan, which is correct regardless of order/overlap.
    sorted: bool,
    /// Max slice END (start+dur, ns) appended so far. A subsequent `start` below
    /// this signals out-of-order arrival or overlap → clears `sorted`.
    max_end: i64,
    /// Evict oldest rows past this (live-view-only); `None` keeps everything.
    cap_rows: Option<usize>,
    /// Oldest retained slice start (ns) when a cap is active — the live retention
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
    let mut pending: Vec<Slice> = Vec::new();
    // Bounded pool for the O(total) reads/flushes we run off the ingest thread.
    let pool = spawn_read_pool(READ_WORKERS);

    loop {
        match rx.recv_timeout(FLUSH_IDLE) {
            Ok(Cmd::Slices(mut v)) => {
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
                // Write off-thread on a cheap (Arc) snapshot so ingest + queries
                // keep flowing during the O(n) serialization. Same bounded pool as
                // reads (a flush is just another off-thread job).
                let snapshot = tl.df.clone();
                let gc = tl.gc.clone();
                let _ = pool.send(Box::new(move || {
                    let _ = reply.send(write_to_file(snapshot.as_ref(), &gc, &path, pid, epoch_ms));
                }));
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

/// Append the pending slices to the timeline as a new chunk; keep `starts` in
/// sync, evict past the cap, and rechunk when fragmented.
fn flush_pending(tl: &mut Timeline, pending: &mut Vec<Slice>) {
    if pending.is_empty() {
        return;
    }
    let n = pending.len();
    match build_df(pending) {
        Ok(batch) => {
            // Append starts in row order and clear `sorted` if any span starts
            // before the max END seen so far — out-of-order arrival OR an overlap
            // with an earlier span (concurrent runtime threads). Either way the
            // contiguous-range window optimization no longer holds. (Checking
            // against max_end, not max_start, catches sorted-but-overlapping spans.)
            for s in pending.iter() {
                let st = s.start as i64;
                if st < tl.max_end {
                    tl.sorted = false;
                }
                tl.max_end = tl.max_end.max(st + s.dur as i64);
                tl.starts.push(st);
            }
            match &mut tl.df {
                Some(existing) => {
                    if let Err(e) = existing.vstack_mut(&batch) {
                        error!(error = %e, "Polars vstack failed");
                    }
                }
                None => tl.df = Some(batch),
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
        "flushed pending slices to DataFrame"
    );
}

/// Build a DataFrame from a batch of slices (computes is_hub once, here).
fn build_df(slices: &[Slice]) -> Result<DataFrame> {
    let n = slices.len();
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
    for s in slices {
        start.push(s.start as i64);
        dur.push(s.dur as i64);
        gid.push(s.gid as i64);
        name.push(s.name.clone());
        func.push(s.func.clone());
        task.push(s.task.clone());
        stack.push(s.stack.clone());
        is_hub.push(
            s.name
                .get(..3)
                .is_some_and(|p| p.eq_ignore_ascii_case("hub")),
        );
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
        Query::Window { .. } => unreachable!("window runs inline on the DB thread"),
    }
}

/// Predicate: slice overlaps `[t0, t1]` (start < t1 AND start+dur > t0).
/// Times are clamped to i64 (the column type) so a u64::MAX sentinel can't wrap
/// negative and exclude everything.
fn overlaps(t0: u64, t1: u64) -> Expr {
    let cap = i64::MAX as u64;
    col("start")
        .lt(lit(t1.min(cap) as i64))
        .and((col("start") + col("dur")).gt(lit(t0.min(cap) as i64)))
}

/// A slice plus its is_hub flag (needed for CPU/track rollups).
struct Row {
    s: Slice,
    is_hub: bool,
}

fn extract_rows(df: &DataFrame) -> Result<Vec<Row>> {
    let h = df.height();
    let start = df.column("start")?.i64()?;
    let dur = df.column("dur")?.i64()?;
    let gid = df.column("gid")?.i64()?;
    let name = df.column("name")?.str()?;
    let func = df.column("func")?.str()?;
    let task = df.column("task")?.str()?;
    let stack = df.column("stack")?.str()?;
    let is_hub = df.column("is_hub")?.bool()?;
    let mut out = Vec::with_capacity(h);
    for i in 0..h {
        out.push(Row {
            s: Slice {
                start: start.get(i).unwrap_or(0) as u64,
                dur: dur.get(i).unwrap_or(0) as u64,
                gid: gid.get(i).unwrap_or(0) as u64,
                name: name.get(i).unwrap_or("").to_string(),
                func: func.get(i).unwrap_or("").to_string(),
                task: task.get(i).unwrap_or("").to_string(),
                stack: stack.get(i).unwrap_or("").to_string(),
            },
            is_hub: is_hub.get(i).unwrap_or(false),
        });
    }
    Ok(out)
}

fn window(tl: &Timeline, t0: u64, t1: u64, cap: usize) -> Result<Reply> {
    let (all, capped) = if tl.sorted {
        window_contiguous(tl, t0, t1, cap)?
    } else {
        window_scan(tl, t0, t1, cap)?
    };

    let tracks = track_runs(&all);
    let gc_win = tl
        .gc
        .iter()
        .filter(|g| g.start < t1 && g.start.saturating_add(g.dur) > t0)
        .cloned()
        .collect();
    let visible = all.len();
    // Actual ns bounds of what we're returning (computed over rows — correct for
    // both the sorted and the overlap-scan path).
    let min_start = all.iter().map(|r| r.s.start).min().unwrap_or(0);
    let max_end = all
        .iter()
        .map(|r| r.s.start.saturating_add(r.s.dur))
        .max()
        .unwrap_or(0);
    let slices = all.into_iter().map(|r| r.s).collect();
    Ok(Reply::Window {
        slices,
        tracks,
        gc: gc_win,
        visible,
        capped,
        min_start,
        max_end,
    })
}

/// Fast path (single cooperative thread): slices don't overlap and `starts` is
/// ascending, so the rows overlapping `[t0,t1]` are a CONTIGUOUS range — from the
/// slice straddling t0 up to (not incl.) the first slice starting at/after t1.
/// Binary-search it and slice it out, no full scan.
fn window_contiguous(tl: &Timeline, t0: u64, t1: u64, cap: usize) -> Result<(Vec<Row>, bool)> {
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
    let rows = match (&tl.df, len) {
        (Some(df), n) if n > 0 => extract_rows(&df.slice(start_idx as i64, n))?,
        _ => Vec::new(),
    };
    Ok((rows, capped))
}

/// Correct path for multi-thread captures: per-thread spans can overlap and
/// arrive out of start order, so the rows overlapping `[t0,t1]` are NOT a
/// contiguous range. Filter the whole DataFrame by overlap (a Polars scan). Over
/// the cap, take the first `cap` matches (a per-pixel LOD downsample is the next
/// step); `capped` flags the truncation.
fn window_scan(tl: &Timeline, t0: u64, t1: u64, cap: usize) -> Result<(Vec<Row>, bool)> {
    let df = match &tl.df {
        Some(d) => d,
        None => return Ok((Vec::new(), false)),
    };
    let out = df
        .clone()
        .lazy()
        .filter(overlaps(t0, t1))
        .collect()
        .context("window overlap scan")?;
    let mut rows = extract_rows(&out)?;
    let capped = rows.len() > cap;
    if capped {
        rows.truncate(cap);
    }
    Ok((rows, capped))
}

/// Sum run-time per track over the window (for activity sort + labels).
fn track_runs(rows: &[Row]) -> Vec<TrackRun> {
    use std::collections::HashMap;
    let mut by: HashMap<u64, (String, bool, u64)> = HashMap::new();
    for r in rows {
        let e = by
            .entry(r.s.gid)
            .or_insert_with(|| (r.s.name.clone(), r.is_hub, 0));
        e.2 = e.2.saturating_add(r.s.dur);
    }
    by.into_iter()
        .map(|(gid, (name, is_hub, run_ns))| TrackRun {
            gid,
            name,
            is_hub,
            run_ns,
        })
        .collect()
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
    // count + rows are correct even when another tier dominates the newest spans.
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

    // `total` is the real number of matching slow spans (drives the badge);
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

// ── Streaming file write ────────────────────────────────────────────────────

/// Stream all rows to a binary `.glr`, decoding at most one chunk at a time so a
/// multi-million-row table never lands in one `Vec`. Returns on-disk size.
fn write_to_file(
    df: Option<&DataFrame>,
    gc: &[GcEvent],
    path: &Path,
    pid: i32,
    epoch_ms: Option<u64>,
) -> Result<u64> {
    let mut writer = crate::record::GlrWriter::create(path, pid, epoch_ms, gc)?;
    let rows = df.map(|d| d.height()).unwrap_or(0);
    if let Some(df) = df {
        let h = df.height();
        let mut off = 0usize;
        while off < h {
            let len = crate::record::CHUNK.min(h - off);
            let part = df.slice(off as i64, len);
            for row in extract_rows(&part)? {
                writer.push_slice(&row.s)?;
            }
            off += len;
        }
    }
    let bytes = writer.finish()?;
    debug!(
        rows,
        gc = gc.len(),
        encoded_bytes = bytes,
        path = %path.display(),
        "writing .glr recording"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    //! Core data-layer tests: drive the real DB thread through its public API
    //! (ingest → query) and assert the window/slowlog/stats contracts the server
    //! relies on. Run with `cargo test`.
    use super::*;
    use crate::store::{GcEvent, Slice};

    const MS: u64 = 1_000_000; // ns per ms

    fn sl(gid: u64, start_ms: u64, dur_ms: u64, name: &str) -> Slice {
        Slice {
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
        db.ingest_slices(vec![sl(1, 5, 10, "Greenlet-1"), sl(2, 20, 5, "Greenlet-2")]);
        assert_eq!(db.total(), 2);
        assert_eq!(db.origin(), 5 * MS); // first slice's start
        assert_eq!(db.span(), 25 * MS); // max(start+dur) = 20+5
        db.set_epoch(1700);
        assert_eq!(db.epoch(), Some(1700));
    }

    #[tokio::test]
    async fn window_handles_overlapping_out_of_order_spans() {
        // Multi-thread capture: a long span on one thread (gid 1, [0,100]) closes
        // — and so ingests — AFTER a later-starting, shorter span on another thread
        // (gid 2, [10,15]). Starts arrive non-monotonic (10 then 0), so the DB must
        // switch off the contiguous fast path and still return BOTH spans for a
        // window inside the overlap.
        let db = Db::spawn(None).unwrap();
        db.ingest_slices(vec![
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
            Reply::Window { slices, .. } => {
                let gids: Vec<u64> = slices.iter().map(|s| s.gid).collect();
                assert!(
                    gids.contains(&1),
                    "long overlapping span must not be dropped"
                );
                assert!(gids.contains(&2), "short span overlapping the window");
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn window_handles_sorted_but_overlapping_spans() {
        // Starts arrive MONOTONIC (0 then 3) yet the spans OVERLAP: a short span
        // gid 1 [0,5] then a long gid 2 [3,103] (its later end ingested after). The
        // start-order check alone wouldn't notice, but tracking max_end does — the
        // DB must scan, not binary-search a contiguous range, or it would drop the
        // earlier long-lived span for a window past its start.
        let db = Db::spawn(None).unwrap();
        db.ingest_slices(vec![
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
            Reply::Window { slices, .. } => {
                let gids: Vec<u64> = slices.iter().map(|s| s.gid).collect();
                assert!(
                    gids.contains(&1),
                    "earlier overlapping span must not be dropped"
                );
                assert!(gids.contains(&2), "later long span overlapping the window");
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn window_returns_only_overlapping_slices() {
        let db = Db::spawn(None).unwrap();
        // Slices always arrive in ascending start order (the collector closes them
        // in switch order, and time only moves forward) — the window index relies
        // on that, so the test ingests them sorted by start.
        db.ingest_slices(vec![
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
                slices,
                visible,
                capped,
                ..
            } => {
                assert!(!capped);
                assert_eq!(visible, slices.len());
                let gids: Vec<u64> = slices.iter().map(|s| s.gid).collect();
                assert!(gids.contains(&3));
                assert!(!gids.contains(&1)); // ended at 10, before 20
                assert!(!gids.contains(&2)); // starts at 50, after 40
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn window_cap_truncates_and_flags() {
        let db = Db::spawn(None).unwrap();
        db.ingest_slices((0..5).map(|i| sl(i, i * 2, 1, "Greenlet")).collect());
        let r = db
            .query(Query::Window {
                t0: 0,
                t1: u64::MAX >> 1,
                cap: 2,
            })
            .await
            .unwrap();
        match r {
            Reply::Window { slices, capped, .. } => {
                assert_eq!(slices.len(), 2);
                assert!(capped);
            }
            _ => panic!("expected Window reply"),
        }
    }

    #[tokio::test]
    async fn slowlog_filters_by_threshold_and_tier() {
        let db = Db::spawn(None).unwrap();
        db.ingest_slices(vec![
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
        let mut slices: Vec<Slice> = (1..=100).map(|i| sl(i, i, 10, "Greenlet")).collect();
        slices.push(sl(999, 0, 100_000, "Hub")); // huge, but Hub → must not skew
        db.ingest_slices(slices);
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
    async fn window_includes_gc_in_range() {
        let db = Db::spawn(None).unwrap();
        db.ingest_slices(vec![sl(1, 0, 100, "Greenlet-1")]);
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
