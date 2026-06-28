//! Shared timeline value types.
//!
//! These are the unit of data everywhere: parsed off the wire in the collector,
//! ingested into the DuckDB-backed [`Db`](crate::db::Db), serialized into `.glr`
//! recordings, and sent to the viewer. The authoritative store is now the
//! embedded database (see `db.rs`); this module only holds the value types.

use serde::{Deserialize, Serialize};

/// One greenlet run-interval: it ran from `start` for `dur` nanoseconds
/// (both relative to the bootstrap's `t0`).
#[derive(Clone, Serialize, Deserialize)]
pub struct Execution {
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
