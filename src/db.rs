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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use polars::prelude::*;
use serde::Serialize;
use tokio::sync::oneshot;
use tracing::error;

use crate::store::{GcEvent, Slice};

/// Sentinel for "origin not yet observed".
const ORIGIN_UNSET: u64 = u64::MAX;
/// Append the ingest buffer to the DataFrame once it reaches this many rows…
const FLUSH_ROWS: usize = 16_384;
/// …or after this long with no command (keeps queries seeing fresh data).
const FLUSH_IDLE: Duration = Duration::from_millis(50);

// ── Query / reply types (also the wire shape — hence Serialize) ─────────────

/// A read request handed to the DB thread.
pub enum Query {
    /// Slices overlapping `[t0, t1]` ns, plus CPU/track/GC rollups for that window.
    Window {
        t0: u64,
        t1: u64,
        /// Hard cap on returned slices (memory bound); `capped` flags truncation.
        cap: usize,
        /// Number of CPU buckets to compute across the window (≈ canvas px).
        buckets: usize,
    },
    /// Newest slow spans (non-Hub), filtered + ordered for the slow-log panel.
    Slowlog {
        warn_ns: u64,
        red_ns: u64,
        red_only: bool,
        sort_dur: bool,
        limit: usize,
    },
    /// Duration percentiles (non-Hub) over `[t0, t1]` (whole timeline if full range).
    Stats { t0: u64, t1: u64 },
}

/// One CPU bucket: `t` is the bucket's left edge (ms since origin), `busy` is the
/// non-Hub run fraction in `[0,1]`.
#[derive(Serialize)]
pub struct CpuBin {
    pub t: f64,
    pub busy: f64,
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
        cpu: Vec<CpuBin>,
        tracks: Vec<TrackRun>,
        gc: Vec<GcEvent>,
        visible: usize,
        capped: bool,
    },
    Slowlog(Vec<SlowRow>),
    Stats { p50: f64, p95: f64, p99: f64 },
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
}

impl Db {
    /// Spawn the DB thread and return a handle.
    pub fn spawn() -> Result<Db> {
        let (tx, rx) = channel::<Cmd>();
        std::thread::Builder::new()
            .name("greenlane-db".into())
            .spawn(move || db_thread(rx))
            .context("spawning DB thread")?;
        Ok(Db {
            tx,
            total: Arc::new(AtomicU64::new(0)),
            bytes: Arc::new(AtomicU64::new(0)),
            span: Arc::new(AtomicU64::new(0)),
            origin: Arc::new(AtomicU64::new(ORIGIN_UNSET)),
            epoch: Arc::new(AtomicU64::new(0)),
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
        let _ = self.tx.send(Cmd::Slices(slices));
    }

    pub fn ingest_gc(&self, gc: Vec<GcEvent>) {
        if gc.is_empty() {
            return;
        }
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

    /// Run a read query on the DB thread.
    pub async fn query(&self, q: Query) -> Result<Reply> {
        let (rtx, rrx) = oneshot::channel();
        self.tx
            .send(Cmd::Query(q, rtx))
            .map_err(|_| anyhow!("DB thread is gone"))?;
        rrx.await.map_err(|_| anyhow!("DB thread dropped the reply"))?
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
        rx.recv().map_err(|_| anyhow!("DB thread dropped the reply"))?
    }
}

// ── DB thread ─────────────────────────────────────────────────────────────

fn db_thread(rx: Receiver<Cmd>) {
    let mut df: Option<DataFrame> = None; // the timeline (columnar)
    let mut gc: Vec<GcEvent> = Vec::new(); // small; kept plain
    let mut pending: Vec<Slice> = Vec::new();

    loop {
        match rx.recv_timeout(FLUSH_IDLE) {
            Ok(Cmd::Slices(mut v)) => {
                pending.append(&mut v);
                if pending.len() >= FLUSH_ROWS {
                    flush_pending(&mut df, &mut pending);
                }
            }
            Ok(Cmd::Gc(mut v)) => gc.append(&mut v),
            Ok(Cmd::Query(q, reply)) => {
                flush_pending(&mut df, &mut pending);
                let _ = reply.send(run_query(df.as_ref(), &gc, q));
            }
            Ok(Cmd::Flush {
                path,
                pid,
                epoch_ms,
                reply,
            }) => {
                flush_pending(&mut df, &mut pending);
                let _ = reply.send(write_to_file(df.as_ref(), &gc, &path, pid, epoch_ms));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                flush_pending(&mut df, &mut pending);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
}

/// Append the pending slices to the timeline DataFrame as a new chunk.
fn flush_pending(df: &mut Option<DataFrame>, pending: &mut Vec<Slice>) {
    if pending.is_empty() {
        return;
    }
    match build_df(pending) {
        Ok(batch) => match df {
            Some(existing) => {
                if let Err(e) = existing.vstack_mut(&batch) {
                    error!(error = %e, "Polars vstack failed");
                }
            }
            None => *df = Some(batch),
        },
        Err(e) => error!(error = %format!("{e:#}"), "building Polars batch failed"),
    }
    pending.clear();
}

/// Build a DataFrame from a batch of slices (computes is_hub once, here).
fn build_df(slices: &[Slice]) -> Result<DataFrame> {
    let n = slices.len();
    let (mut start, mut dur, mut gid) =
        (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut name, mut func, mut task, mut stack) =
        (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    let mut is_hub = Vec::with_capacity(n);
    for s in slices {
        start.push(s.start as i64);
        dur.push(s.dur as i64);
        gid.push(s.gid as i64);
        name.push(s.name.clone());
        func.push(s.func.clone());
        task.push(s.task.clone());
        stack.push(s.stack.clone());
        is_hub.push(s.name.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("hub")));
    }
    df!(
        "start" => start, "dur" => dur, "gid" => gid,
        "name" => name, "func" => func, "task" => task, "stack" => stack,
        "is_hub" => is_hub,
    )
    .context("building timeline DataFrame")
}

// ── Queries ─────────────────────────────────────────────────────────────────

fn run_query(df: Option<&DataFrame>, gc: &[GcEvent], q: Query) -> Result<Reply> {
    match q {
        Query::Window { t0, t1, cap, buckets } => window(df, gc, t0, t1, cap, buckets),
        Query::Slowlog { warn_ns, red_ns, red_only, sort_dur, limit } => {
            slowlog(df, warn_ns, red_ns, red_only, sort_dur, limit)
        }
        Query::Stats { t0, t1 } => stats(df, t0, t1),
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

fn window(
    df: Option<&DataFrame>,
    gc: &[GcEvent],
    t0: u64,
    t1: u64,
    cap: usize,
    buckets: usize,
) -> Result<Reply> {
    // Fetch one extra row to detect truncation. Rows stay start-ordered (ingest
    // order ≈ start order), so no explicit sort is needed.
    let mut all = match df {
        None => Vec::new(),
        Some(df) => {
            let out = df
                .clone()
                .lazy()
                .filter(overlaps(t0, t1))
                .limit((cap as u32) + 1)
                .collect()
                .context("window query")?;
            extract_rows(&out)?
        }
    };
    let capped = all.len() > cap;
    if capped {
        all.truncate(cap);
    }

    let cpu = cpu_bins(&all, t0, t1, buckets);
    let tracks = track_runs(&all);
    let gc_win = gc
        .iter()
        .filter(|g| g.start < t1 && g.start.saturating_add(g.dur) > t0)
        .cloned()
        .collect();
    let visible = all.len();
    let slices = all.into_iter().map(|r| r.s).collect();
    Ok(Reply::Window {
        slices,
        cpu,
        tracks,
        gc: gc_win,
        visible,
        capped,
    })
}

/// Bucket non-Hub run-time into `buckets` slots across `[t0,t1]`; busy = fraction.
fn cpu_bins(rows: &[Row], t0: u64, t1: u64, buckets: usize) -> Vec<CpuBin> {
    let buckets = buckets.max(1);
    let span = t1.saturating_sub(t0).max(1) as f64;
    let w = span / buckets as f64; // bucket width in ns
    let mut busy = vec![0f64; buckets];
    for r in rows {
        if r.is_hub {
            continue;
        }
        let s = r.s.start.max(t0) as f64;
        let e = (r.s.start.saturating_add(r.s.dur)).min(t1) as f64;
        if e <= s {
            continue;
        }
        let mut b = (((s - t0 as f64) / w).floor() as isize).max(0) as usize;
        while b < buckets {
            let bs = t0 as f64 + b as f64 * w;
            if bs >= e {
                break;
            }
            busy[b] += (e.min(bs + w) - s.max(bs)).max(0.0);
            b += 1;
        }
    }
    busy.into_iter()
        .enumerate()
        .map(|(i, ns)| CpuBin {
            t: (t0 as f64 + i as f64 * w) / 1e6,
            busy: (ns / w).clamp(0.0, 1.0),
        })
        .collect()
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
        .map(|(gid, (name, is_hub, run_ns))| TrackRun { gid, name, is_hub, run_ns })
        .collect()
}

fn slowlog(
    df: Option<&DataFrame>,
    warn_ns: u64,
    red_ns: u64,
    red_only: bool,
    sort_dur: bool,
    limit: usize,
) -> Result<Reply> {
    let threshold = if red_only { red_ns } else { warn_ns };
    let mut rows: Vec<SlowRow> = match df {
        None => Vec::new(),
        Some(df) => {
            let out = df
                .clone()
                .lazy()
                .filter(col("is_hub").not().and(col("dur").gt(lit(threshold as i64))))
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
            (0..h)
                .map(|i| {
                    let d = dur.get(i).unwrap_or(0) as u64;
                    SlowRow {
                        start: start.get(i).unwrap_or(0) as u64,
                        dur: d,
                        gid: gid.get(i).unwrap_or(0) as u64,
                        name: name.get(i).unwrap_or("").to_string(),
                        func: func.get(i).unwrap_or("").to_string(),
                        level: if d > red_ns { 2 } else { 1 },
                    }
                })
                .collect()
        }
    };
    if sort_dur {
        rows.sort_by_key(|r| std::cmp::Reverse(r.dur)); // longest first
    } else {
        rows.sort_by_key(|r| std::cmp::Reverse(r.start)); // newest first
    }
    rows.truncate(limit);
    Ok(Reply::Slowlog(rows))
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
                    col("dur").quantile(lit(0.5), QuantileMethod::Linear).alias("p50"),
                    col("dur").quantile(lit(0.95), QuantileMethod::Linear).alias("p95"),
                    col("dur").quantile(lit(0.99), QuantileMethod::Linear).alias("p99"),
                ])
                .collect()
                .context("stats query")?;
            (get_f64(&out, "p50"), get_f64(&out, "p95"), get_f64(&out, "p99"))
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

// ── Streaming file write ────────────────────────────────────────────────────

/// Stream all rows to a chunked `.glr`, decoding at most one chunk at a time so
/// a multi-million-row table never lands in one `Vec`. Returns on-disk size.
fn write_to_file(
    df: Option<&DataFrame>,
    gc: &[GcEvent],
    path: &Path,
    pid: i32,
    epoch_ms: Option<u64>,
) -> Result<u64> {
    let mut chunk_lens: Vec<u32> = Vec::new();
    let mut chunk_data: Vec<u8> = Vec::new();
    if let Some(df) = df {
        let h = df.height();
        let mut off = 0usize;
        while off < h {
            let len = crate::record::CHUNK.min(h - off);
            let part = df.slice(off as i64, len);
            let slices: Vec<Slice> = extract_rows(&part)?.into_iter().map(|r| r.s).collect();
            let bytes = crate::record::encode_chunk(&slices)?;
            chunk_lens.push(bytes.len() as u32);
            chunk_data.extend_from_slice(&bytes);
            off += len;
        }
    }
    crate::record::write_file(path, pid, epoch_ms, gc.to_vec(), chunk_lens, &chunk_data)
}
