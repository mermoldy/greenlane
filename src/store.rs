//! Authoritative, server-side timeline store.
//!
//! The collector closes one [`Slice`] per greenlet run-interval and pushes it
//! here. Each viewer holds a cursor (count of slices it has received) and the
//! server hands it the contiguous tail since that cursor on a fixed timer. This
//! lossless cursor model replaces a broadcast channel: no per-client lag, no
//! dropped events, and therefore no expensive full-snapshot resyncs.
//!
//! This is also the seam where server-side LOD will live: today `delta()`
//! returns raw slices (fat-client v1); a future `query(viewport)` will return
//! per-pixel aggregates so the wire and the browser stay bounded by the screen.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// One greenlet run-interval: it ran from `start` for `dur` nanoseconds
/// (both relative to the bootstrap's `t0`).
#[derive(Clone, Serialize, Deserialize)]
pub struct Slice {
    /// Stable greenlet identity (Python `id()` of the object).
    pub gid: u64,
    /// Start time in ns since the trace began.
    pub start: u64,
    /// Duration in ns.
    pub dur: u64,
    /// Human label, e.g. "Hub", "Greenlet-3".
    pub name: String,
    /// App function the greenlet resumed into ("file.py:qualname:lineno"), or "".
    pub func: String,
    /// App-set correlation id (request_id / task_id / trace_id), or "".
    pub task: String,
    /// Call chain (leaf → root, " <- " joined) of the resuming greenlet, or "".
    pub stack: String,
}

/// A garbage-collection pause: a global stall of the whole gevent thread.
#[derive(Clone, Serialize, Deserialize)]
pub struct GcEvent {
    /// Start time in ns since the trace began.
    pub start: u64,
    /// Pause duration in ns.
    pub dur: u64,
    /// GC generation collected (0/1/2).
    #[serde(rename = "gen")]
    pub generation: i64,
    /// Objects collected.
    pub collected: i64,
}

/// A complete, self-contained timeline — the on-disk recording format. This is
/// exactly the state a [`Store`] holds, flattened for serialization; `open`
/// rebuilds a `Store` from it and serves the same viewer in static mode.
#[derive(Serialize, Deserialize)]
pub struct Recording {
    /// PID the recording was captured from.
    pub pid: i32,
    /// Wall-clock epoch (ms) at trace t0, or `None` if it was never reported.
    pub epoch_ms: Option<u64>,
    pub slices: Vec<Slice>,
    pub gc: Vec<GcEvent>,
}

pub struct Store {
    slices: Mutex<Vec<Slice>>,
    gc: Mutex<Vec<GcEvent>>,
    /// Wall-clock epoch (ms) corresponding to trace t0, for absolute time modes.
    epoch_ms: Mutex<Option<u64>>,
    /// Total raw bytes read from the target's event stream — the volume of data
    /// greenlane has processed this session. Reported live in the viewer header.
    bytes: AtomicU64,
}

impl Store {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slices: Mutex::new(Vec::new()),
            gc: Mutex::new(Vec::new()),
            epoch_ms: Mutex::new(None),
            bytes: AtomicU64::new(0),
        })
    }

    /// Rebuild a store from a loaded recording (the `open` path). A recording
    /// was loaded from disk, not streamed, so its live byte counter starts at 0
    /// (the viewer shows the on-disk file size for recordings instead).
    pub fn from_recording(rec: Recording) -> Arc<Self> {
        Arc::new(Self {
            slices: Mutex::new(rec.slices),
            gc: Mutex::new(rec.gc),
            epoch_ms: Mutex::new(rec.epoch_ms),
            bytes: AtomicU64::new(0),
        })
    }

    /// Add to the running total of raw event-stream bytes read from the target.
    pub fn add_bytes(&self, n: usize) {
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Total raw event-stream bytes processed so far.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Snapshot the whole timeline into a serializable [`Recording`].
    pub fn export(&self, pid: i32) -> Recording {
        Recording {
            pid,
            epoch_ms: *self.epoch_ms.lock().unwrap(),
            slices: self.slices.lock().unwrap().clone(),
            gc: self.gc.lock().unwrap().clone(),
        }
    }

    /// Append a closed slice.
    pub fn push(&self, slice: Slice) {
        self.slices.lock().unwrap().push(slice);
    }

    /// Number of closed slices recorded so far.
    pub fn slice_count(&self) -> usize {
        self.slices.lock().unwrap().len()
    }

    pub fn push_gc(&self, ev: GcEvent) {
        self.gc.lock().unwrap().push(ev);
    }

    /// GC events appended since `cursor`, plus the new cursor.
    pub fn gc_delta(&self, cursor: usize) -> (Vec<GcEvent>, usize) {
        let g = self.gc.lock().unwrap();
        let len = g.len();
        let batch = if cursor < len {
            g[cursor..].to_vec()
        } else {
            Vec::new()
        };
        (batch, len)
    }

    pub fn set_epoch(&self, ms: u64) {
        *self.epoch_ms.lock().unwrap() = Some(ms);
    }

    pub fn epoch(&self) -> Option<u64> {
        *self.epoch_ms.lock().unwrap()
    }

    /// The slices appended since `cursor`, plus the new cursor (current length).
    /// Clones only the tail, so steady-state cost is proportional to new data.
    pub fn delta(&self, cursor: usize) -> (Vec<Slice>, usize) {
        let g = self.slices.lock().unwrap();
        let len = g.len();
        let batch = if cursor < len {
            g[cursor..].to_vec()
        } else {
            Vec::new()
        };
        (batch, len)
    }
}
