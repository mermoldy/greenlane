//! greenlane binary trace format (`.glr` / wire) — encoder + decoder.
//!
//! One framed binary format is used both on the live Unix-socket wire (target
//! bootstrap → collector) and in the on-disk `.glr` recording. It replaces the
//! original tab-delimited text stream (see ADR 0001). The format spec lives in
//! [`docs/design/glr-format.md`]; this module is its single Rust implementation.
//! The Python bootstrap carries a byte-compatible encoder (`src/bootstrap.py`,
//! inlined from `src/glr.py`), kept in lockstep with the constants and layout here.
//!
//! ## Layout
//!
//! ```text
//!   Header := Magic(4)=b"GLR\0" Version(1)=1
//!   Frame  := Tag(1) Len(varint) Body[Len]
//! ```
//!
//! Every frame is **length-prefixed** so the streaming reader is trivial (read
//! tag + len, then exactly that many body bytes) and an unknown future tag can be
//! skipped without understanding it. All multi-byte ints are little-endian;
//! variable-length ints are **LEB128** (`u64` ≤ 10 bytes), signed ints **zigzag**
//! then LEB128.
//!
//! ## Frames
//!
//! | Tag    | Name        | Body                                                    |
//! | ------ | ----------- | ------------------------------------------------------- |
//! | `0x01` | Schema      | `type_id:u16 flags:u8 name:istr nfields:v (fname:istr ftype:u8 unit:u8)*` |
//! | `0x02` | Event       | `type_id:u16 [ts_delta:u24 if has_ts] fields…`          |
//! | `0x03` | String pool | `istr` — appends one UTF-8 string (id = next, 0 = `""`)  |
//! | `0x04` | Stack pool  | `n:v (str_id:v)*` — appends one stack (id = next, 0 = ∅) |
//! | `0x05` | Ts reset    | `base:u64` — re-establish the absolute ns timestamp base |
//! | `0x06` | Meta        | `epoch_ms:v tid:v pid:i64 pyinfo:istr`                   |
//!
//! `istr` = inline string = `len:varint utf8[len]`. The string/stack pools are
//! the primary size win: an identical label or call stack is sent once and
//! referenced by a compact id thereafter, which is what makes full traces cheap.
//!
//! ## Event timestamps
//!
//! A `has_ts` event carries a **24-bit unsigned delta** (ns since the running
//! base); after it, `base += delta`. When a gap exceeds the 24-bit range
//! (~16.7 ms) the encoder first emits a Ts-reset frame with a fresh absolute base.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::store::{Execution, GcEvent};

// ── Wire constants (mirrored in the Python bootstraps) ──────────────────────

/// File/stream signature. Distinguishes the binary format from the legacy text
/// `.glr` (which began with `b"GREENLNE"`).
pub const MAGIC: [u8; 4] = *b"GLR\0";
/// Format major version. Decoders reject unknown majors.
pub const VERSION: u8 = 1;

// Frame tags.
const T_SCHEMA: u8 = 0x01;
const T_EVENT: u8 = 0x02;
const T_STRPOOL: u8 = 0x03;
const T_STACKPOOL: u8 = 0x04;
const T_TSRESET: u8 = 0x05;
const T_META: u8 = 0x06;

// Field type tags (low 7 bits; high bit 0x80 reserved for "optional").
const FT_VARINT: u8 = 0x01; // u64 LEB128
const FT_I64: u8 = 0x02; // zigzag LEB128
const FT_PSTR: u8 = 0x03; // string-pool id (varint)
const FT_PSTACK: u8 = 0x04; // stack-pool id (varint)
const FT_BOOL: u8 = 0x05; // 1 byte
const FT_F64: u8 = 0x06; // 8 bytes LE
const FT_OPTIONAL: u8 = 0x80; // marks a field optional (1-byte presence prefix)

/// `Schema.flags` bit: instances carry the packed 24-bit timestamp delta.
const FLAG_TS: u8 = 0x01;

/// Largest representable event timestamp delta (24-bit). A larger gap forces a
/// Ts-reset frame.
const TS_DELTA_MAX: u64 = (1 << 24) - 1;

/// Upper bound on a single frame body. Real frames are tiny (events are tens of
/// bytes; the largest is a pool string), so this is generous — it exists only so a
/// corrupt/hostile length can't make a reader buffer unboundedly (OOM) waiting for
/// a frame that never completes, or overflow when computing the frame end.
const MAX_FRAME_LEN: u64 = 64 << 20; // 64 MiB

// Built-in event type ids. `switch` rides the wire (the collector closes each
// into an Execution); `execution` is the pre-closed interval stored in a `.glr`; `gc`
// appears in both.
pub const TY_SWITCH: u16 = 1;
pub const TY_GC: u16 = 2;
pub const TY_EXECUTION: u16 = 3;

// ── Decoded items ────────────────────────────────────────────────────────────

/// One greenlet/task switch off the wire: the target greenlet resuming at `ts`.
/// `origin` is intentionally absent — the collector never used it. The collector
/// closes the previous interval on the same `thread` and opens this one.
pub struct Switch {
    pub ts: u64,
    pub target: u64,
    pub label: String,
    pub func: String,
    pub task: String,
    pub stack: String,
    pub thread: u64,
}

/// Session metadata (epoch wall-clock, runtime thread id, pid, interpreter JSON).
pub struct Meta {
    pub epoch_ms: u64,
    pub tid: u64,
    pub pid: i64,
    pub pyinfo: String,
}

/// One decoded frame's payload (control frames — pools, schemas, ts-reset, and
/// unknown event types — decode to `None` and just mutate decoder state).
pub enum Item {
    Meta(Meta),
    Switch(Switch),
    Execution(Execution),
    Gc(GcEvent),
}

// ── Varint / zigzag helpers ──────────────────────────────────────────────────

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Outcome of reading a LEB128 varint: a value, "need more bytes" (the buffer
/// ended mid-varint — not an error for the frame envelope), or "malformed" (it
/// runs past 64 bits, i.e. a corrupt/overlong encoding — always an error).
enum Varint {
    Val(u64),
    NeedMore,
    Overlong,
}

/// Read a LEB128 varint at `*pos`, advancing it past the consumed bytes.
fn peek_varint(buf: &[u8], pos: &mut usize) -> Varint {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = match buf.get(*pos) {
            Some(&b) => b,
            None => return Varint::NeedMore,
        };
        *pos += 1;
        // The final (10th) byte may set at most bit 63: shift==63 leaves room for 1
        // bit, so its payload must be <= 1. Anything wider is overlong/corrupt.
        if shift >= 64 || (shift == 63 && (byte & 0x7f) > 1) {
            return Varint::Overlong;
        }
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Varint::Val(result);
        }
        shift += 7;
    }
}

// ── Body cursor (decodes a single, already-complete frame body) ──────────────

/// A read cursor over one frame body. Every read is bounds-checked; running off
/// the end is a hard decode error (the frame's length said it was complete).
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| anyhow::anyhow!("trace frame truncated (u8)"))?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u24(&mut self) -> Result<u64> {
        let b = self.take(3)?;
        Ok(u64::from(b[0]) | (u64::from(b[1]) << 8) | (u64::from(b[2]) << 16))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn f64(&mut self) -> Result<f64> {
        let b = self.take(8)?;
        Ok(f64::from_le_bytes(b.try_into().unwrap()))
    }

    fn varint(&mut self) -> Result<u64> {
        match peek_varint(self.buf, &mut self.pos) {
            Varint::Val(v) => Ok(v),
            Varint::NeedMore => bail!("trace frame truncated (varint)"),
            Varint::Overlong => bail!("trace frame malformed (overlong varint)"),
        }
    }

    fn ivarint(&mut self) -> Result<i64> {
        Ok(unzigzag(self.varint()?))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| anyhow::anyhow!("trace frame truncated ({n} bytes)"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// Inline string: `len:varint utf8[len]`.
    fn istr(&mut self) -> Result<String> {
        let n = self.varint()? as usize;
        let bytes = self.take(n)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
}

// ── Schema ──────────────────────────────────────────────────────────────────

struct Schema {
    has_ts: bool,
    /// Field type tags in order (names are kept only for round-trip/debug).
    field_types: Vec<u8>,
}

/// A field value read generically from an event body (so unknown event types can
/// still be skipped, and known ones built by field index). `Bool`/`F` are decoded
/// for completeness — no built-in event maps them to an [`Item`] field yet, so
/// their payloads go unread (they only need to consume the right number of bytes).
#[allow(dead_code)]
enum Val {
    U(u64),
    I(i64),
    Str(String),
    Stack(String),
    Bool(bool),
    F(f64),
}

impl Val {
    fn as_u(&self) -> u64 {
        match self {
            Val::U(v) => *v,
            _ => 0,
        }
    }
    fn as_i(&self) -> i64 {
        match self {
            Val::I(v) => *v,
            _ => 0,
        }
    }
    fn into_string(self) -> String {
        match self {
            Val::Str(s) | Val::Stack(s) => s,
            _ => String::new(),
        }
    }
}

// ── Decoder ──────────────────────────────────────────────────────────────────

/// Result of attempting to decode one frame from a byte buffer.
pub enum Step {
    /// Not enough bytes buffered yet for a complete frame — read more, retry.
    NeedMore,
    /// A frame was consumed (`consumed` bytes); `item` is its payload, if any
    /// (control frames produce `None`).
    Done { item: Option<Item>, consumed: usize },
}

/// Streaming decoder. Holds the interning pools, schema registry, and timestamp
/// base; [`step`](Decoder::step) is fed a buffer and consumes one frame at a time,
/// signalling [`Step::NeedMore`] when the buffer holds only a partial frame.
pub struct Decoder {
    strings: Vec<String>,
    stacks: Vec<String>,
    schemas: HashMap<u16, Schema>,
    base: u64,
    header_seen: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            // id 0 is the empty string / empty stack in both pools.
            strings: vec![String::new()],
            stacks: vec![String::new()],
            schemas: HashMap::new(),
            base: 0,
            header_seen: false,
        }
    }

    /// Try to decode one frame (or the leading header) from the front of `buf`.
    pub fn step(&mut self, buf: &[u8]) -> Result<Step> {
        // Header first: Magic(4) + Version(1).
        if !self.header_seen {
            if buf.len() < 5 {
                return Ok(Step::NeedMore);
            }
            if buf[..4] != MAGIC {
                bail!("not a greenlane binary trace (bad magic)");
            }
            if buf[4] != VERSION {
                bail!(
                    "unsupported trace format version {} (this build reads v{})",
                    buf[4],
                    VERSION
                );
            }
            self.header_seen = true;
            return Ok(Step::Done {
                item: None,
                consumed: 5,
            });
        }

        // Frame envelope: Tag(1) Len(varint) Body[Len].
        if buf.is_empty() {
            return Ok(Step::NeedMore);
        }
        let tag = buf[0];
        let mut pos = 1usize;
        let len_u64 = match peek_varint(buf, &mut pos) {
            Varint::Val(l) => l,
            Varint::NeedMore => return Ok(Step::NeedMore), // length varint incomplete
            Varint::Overlong => bail!("malformed frame length (overlong varint) — stream corrupt"),
        };
        // Reject an implausibly large length BEFORE buffering toward it, so a
        // corrupt stream errors out instead of growing the reader's buffer to OOM.
        if len_u64 > MAX_FRAME_LEN {
            bail!("trace frame too large ({len_u64} bytes, cap {MAX_FRAME_LEN}) — stream corrupt");
        }
        let total = pos
            .checked_add(len_u64 as usize)
            .ok_or_else(|| anyhow::anyhow!("trace frame length overflow"))?;
        if buf.len() < total {
            return Ok(Step::NeedMore);
        }
        let body = &buf[pos..total];
        let item = self.decode_body(tag, body)?;
        Ok(Step::Done {
            item,
            consumed: total,
        })
    }

    fn decode_body(&mut self, tag: u8, body: &[u8]) -> Result<Option<Item>> {
        let mut c = Cursor::new(body);
        match tag {
            T_STRPOOL => {
                let s = c.istr()?;
                self.strings.push(s);
                Ok(None)
            }
            T_STACKPOOL => {
                let n = c.varint()? as usize;
                let mut frames: Vec<&str> = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = c.varint()? as usize;
                    let s = self.strings.get(id).ok_or_else(|| {
                        anyhow::anyhow!("stack references undefined string id {id}")
                    })?;
                    frames.push(s);
                }
                self.stacks.push(frames.join(" <- "));
                Ok(None)
            }
            T_TSRESET => {
                self.base = c.u64()?;
                Ok(None)
            }
            T_META => {
                let epoch_ms = c.varint()?;
                let tid = c.varint()?;
                let pid = c.ivarint()?;
                let pyinfo = c.istr()?;
                Ok(Some(Item::Meta(Meta {
                    epoch_ms,
                    tid,
                    pid,
                    pyinfo,
                })))
            }
            T_SCHEMA => {
                self.decode_schema(&mut c)?;
                Ok(None)
            }
            T_EVENT => self.decode_event(&mut c),
            _ => Ok(None), // unknown frame tag — length-prefix lets us skip it
        }
    }

    fn decode_schema(&mut self, c: &mut Cursor) -> Result<()> {
        let type_id = c.u16()?;
        let flags = c.u8()?;
        let _name = c.istr()?;
        let nfields = c.varint()? as usize;
        let mut field_types = Vec::with_capacity(nfields);
        for _ in 0..nfields {
            let _fname = c.istr()?;
            let ftype = c.u8()?;
            let _unit = c.u8()?;
            field_types.push(ftype);
        }
        self.schemas.insert(
            type_id,
            Schema {
                has_ts: flags & FLAG_TS != 0,
                field_types,
            },
        );
        Ok(())
    }

    fn decode_event(&mut self, c: &mut Cursor) -> Result<Option<Item>> {
        let type_id = c.u16()?;
        let schema = self
            .schemas
            .get(&type_id)
            .ok_or_else(|| anyhow::anyhow!("event references undefined schema {type_id}"))?;
        let has_ts = schema.has_ts;
        // Clone the small field-type list so we can read pools (which borrow self)
        // without holding the schema borrow.
        let field_types = schema.field_types.clone();

        let ts = if has_ts {
            let delta = c.u24()?;
            self.base = self.base.wrapping_add(delta);
            self.base
        } else {
            0
        };

        let mut vals: Vec<Val> = Vec::with_capacity(field_types.len());
        for ft in &field_types {
            // Optional fields carry a 1-byte presence prefix.
            if ft & FT_OPTIONAL != 0 && c.u8()? == 0 {
                vals.push(Val::U(0));
                continue;
            }
            let v = match ft & !FT_OPTIONAL {
                FT_VARINT => Val::U(c.varint()?),
                FT_I64 => Val::I(c.ivarint()?),
                FT_BOOL => Val::Bool(c.u8()? != 0),
                FT_F64 => Val::F(c.f64()?),
                FT_PSTR => {
                    let id = c.varint()? as usize;
                    Val::Str(self.string(id)?)
                }
                FT_PSTACK => {
                    let id = c.varint()? as usize;
                    Val::Stack(self.stack(id)?)
                }
                other => bail!("unknown field type tag {other}"),
            };
            vals.push(v);
        }

        Ok(self.build_item(type_id, ts, vals))
    }

    /// Build a known event into an [`Item`]; unknown type ids decode to `None`
    /// (already consumed — forward-compatible skip).
    fn build_item(&self, type_id: u16, ts: u64, mut vals: Vec<Val>) -> Option<Item> {
        match type_id {
            // switch: target, label, func, task, stack, thread
            TY_SWITCH if vals.len() >= 6 => {
                let thread = vals[5].as_u();
                let target = vals[0].as_u();
                let mut it = vals.drain(..);
                let _target = it.next();
                let label = it.next().unwrap().into_string();
                let func = it.next().unwrap().into_string();
                let task = it.next().unwrap().into_string();
                let stack = it.next().unwrap().into_string();
                Some(Item::Switch(Switch {
                    ts,
                    target,
                    label,
                    func,
                    task,
                    stack,
                    thread,
                }))
            }
            // execution (ts = start): dur, gid, name, func, task, stack
            TY_EXECUTION if vals.len() >= 6 => {
                let dur = vals[0].as_u();
                let gid = vals[1].as_u();
                let mut it = vals.drain(2..);
                let name = it.next().unwrap().into_string();
                let func = it.next().unwrap().into_string();
                let task = it.next().unwrap().into_string();
                let stack = it.next().unwrap().into_string();
                Some(Item::Execution(Execution {
                    gid,
                    start: ts,
                    dur,
                    name,
                    func,
                    task,
                    stack,
                }))
            }
            // gc: start, dur, generation, collected
            TY_GC if vals.len() >= 4 => Some(Item::Gc(GcEvent {
                start: vals[0].as_u(),
                dur: vals[1].as_u(),
                generation: vals[2].as_i(),
                collected: vals[3].as_i(),
            })),
            _ => None,
        }
    }

    fn string(&self, id: usize) -> Result<String> {
        self.strings
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("undefined string id {id}"))
    }

    fn stack(&self, id: usize) -> Result<String> {
        self.stacks
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("undefined stack id {id}"))
    }
}

// ── Encoder ──────────────────────────────────────────────────────────────────

/// Builds the framed binary stream. Used by the `.glr` writer (and tests); the
/// Python bootstraps reimplement the same byte layout for the hot-path wire.
pub struct Encoder {
    out: Vec<u8>,
    strings: HashMap<String, u64>,
    stacks: HashMap<String, u64>,
    next_str: u64,
    next_stack: u64,
    base: u64,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    /// Start a stream: writes the header. Pre-seeds pool id 0 = empty.
    pub fn new() -> Self {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        let mut strings = HashMap::new();
        strings.insert(String::new(), 0);
        let mut stacks = HashMap::new();
        stacks.insert(String::new(), 0);
        Encoder {
            out,
            strings,
            stacks,
            next_str: 1,
            next_stack: 1,
            base: 0,
        }
    }

    /// Append a length-prefixed frame from an already-built body.
    fn frame(&mut self, tag: u8, body: &[u8]) {
        self.out.push(tag);
        write_varint(&mut self.out, body.len() as u64);
        self.out.extend_from_slice(body);
    }

    /// Intern a string, emitting a String-pool frame on first sight; returns its id.
    fn intern_str(&mut self, s: &str) -> u64 {
        if let Some(&id) = self.strings.get(s) {
            return id;
        }
        let id = self.next_str;
        self.next_str += 1;
        self.strings.insert(s.to_string(), id);
        let mut body = Vec::new();
        write_varint(&mut body, s.len() as u64);
        body.extend_from_slice(s.as_bytes());
        self.frame(T_STRPOOL, &body);
        id
    }

    /// Intern a `" <- "`-joined stack, emitting a Stack-pool frame on first sight.
    fn intern_stack(&mut self, joined: &str) -> u64 {
        if joined.is_empty() {
            return 0;
        }
        if let Some(&id) = self.stacks.get(joined) {
            return id;
        }
        // Frame strings must be interned (and their pool frames emitted) first.
        let frame_ids: Vec<u64> = joined.split(" <- ").map(|f| self.intern_str(f)).collect();
        let id = self.next_stack;
        self.next_stack += 1;
        self.stacks.insert(joined.to_string(), id);
        let mut body = Vec::new();
        write_varint(&mut body, frame_ids.len() as u64);
        for fid in frame_ids {
            write_varint(&mut body, fid);
        }
        self.frame(T_STACKPOOL, &body);
        id
    }

    /// Emit a schema frame. `fields` is `(name, type_tag, unit)`.
    pub fn schema(&mut self, type_id: u16, has_ts: bool, name: &str, fields: &[(&str, u8, u8)]) {
        let mut body = Vec::new();
        body.extend_from_slice(&type_id.to_le_bytes());
        body.push(if has_ts { FLAG_TS } else { 0 });
        write_varint(&mut body, name.len() as u64);
        body.extend_from_slice(name.as_bytes());
        write_varint(&mut body, fields.len() as u64);
        for (fname, ftype, unit) in fields {
            write_varint(&mut body, fname.len() as u64);
            body.extend_from_slice(fname.as_bytes());
            body.push(*ftype);
            body.push(*unit);
        }
        self.frame(T_SCHEMA, &body);
    }

    /// Emit the built-in `execution` + `gc` schemas (the set a `.glr` uses).
    pub fn write_file_schemas(&mut self) {
        self.schema(
            TY_EXECUTION,
            true,
            "execution",
            &[
                ("dur", FT_VARINT, UNIT_NS),
                ("gid", FT_VARINT, UNIT_NONE),
                ("name", FT_PSTR, UNIT_NONE),
                ("func", FT_PSTR, UNIT_NONE),
                ("task", FT_PSTR, UNIT_NONE),
                ("stack", FT_PSTACK, UNIT_NONE),
            ],
        );
        self.schema(
            TY_GC,
            false,
            "gc",
            &[
                ("start", FT_VARINT, UNIT_NS),
                ("dur", FT_VARINT, UNIT_NS),
                ("generation", FT_I64, UNIT_NONE),
                ("collected", FT_I64, UNIT_NONE),
            ],
        );
    }

    pub fn meta(&mut self, epoch_ms: u64, tid: u64, pid: i64, pyinfo: &str) {
        let mut body = Vec::new();
        write_varint(&mut body, epoch_ms);
        write_varint(&mut body, tid);
        write_varint(&mut body, zigzag(pid));
        write_varint(&mut body, pyinfo.len() as u64);
        body.extend_from_slice(pyinfo.as_bytes());
        self.frame(T_META, &body);
    }

    /// Compute the packed timestamp delta for `ts`, emitting a Ts-reset frame when
    /// the gap exceeds the 24-bit range.
    fn ts_delta(&mut self, ts: u64) -> u64 {
        let delta = ts.wrapping_sub(self.base);
        if ts < self.base || delta > TS_DELTA_MAX {
            let mut body = Vec::new();
            body.extend_from_slice(&ts.to_le_bytes());
            self.frame(T_TSRESET, &body);
            self.base = ts;
            0
        } else {
            self.base = ts;
            delta
        }
    }

    /// Append a `execution` event (ts = start).
    pub fn execution(&mut self, s: &Execution) {
        let func = self.intern_str(&s.func);
        let name = self.intern_str(&s.name);
        let task = self.intern_str(&s.task);
        let stack = self.intern_stack(&s.stack);
        let delta = self.ts_delta(s.start);
        let mut body = Vec::new();
        body.extend_from_slice(&TY_EXECUTION.to_le_bytes());
        body.extend_from_slice(&[
            (delta & 0xff) as u8,
            ((delta >> 8) & 0xff) as u8,
            ((delta >> 16) & 0xff) as u8,
        ]);
        write_varint(&mut body, s.dur);
        write_varint(&mut body, s.gid);
        write_varint(&mut body, name);
        write_varint(&mut body, func);
        write_varint(&mut body, task);
        write_varint(&mut body, stack);
        self.frame(T_EVENT, &body);
    }

    /// Append a `gc` event.
    pub fn gc(&mut self, g: &GcEvent) {
        let mut body = Vec::new();
        body.extend_from_slice(&TY_GC.to_le_bytes());
        write_varint(&mut body, g.start);
        write_varint(&mut body, g.dur);
        write_varint(&mut body, zigzag(g.generation));
        write_varint(&mut body, zigzag(g.collected));
        self.frame(T_EVENT, &body);
    }

    /// Emit the `switch` + `gc` schemas the live wire stream uses (mirror of the
    /// Python `glr.write_wire_schemas`). Field order must match the decoder.
    /// (Rust wire-encode parity with `glr.py`; currently exercised by tests.)
    #[allow(dead_code)]
    pub fn write_wire_schemas(&mut self) {
        self.schema(
            TY_SWITCH,
            true,
            "switch",
            &[
                ("target", FT_VARINT, UNIT_NONE),
                ("label", FT_PSTR, UNIT_NONE),
                ("func", FT_PSTR, UNIT_NONE),
                ("task", FT_PSTR, UNIT_NONE),
                ("stack", FT_PSTACK, UNIT_NONE),
                ("thread", FT_VARINT, UNIT_NONE),
            ],
        );
        self.schema(
            TY_GC,
            false,
            "gc",
            &[
                ("start", FT_VARINT, UNIT_NS),
                ("dur", FT_VARINT, UNIT_NS),
                ("generation", FT_I64, UNIT_NONE),
                ("collected", FT_I64, UNIT_NONE),
            ],
        );
    }

    /// Append a `switch` event (ts = when the switch occurred), the way the live
    /// bootstrap streams them. Mirrors `glr.switch` / the `execution` encoder above.
    #[allow(clippy::too_many_arguments, dead_code)]
    pub fn switch(
        &mut self,
        ts: u64,
        target: u64,
        label: &str,
        func: &str,
        task: &str,
        stack: &str,
        thread: u64,
    ) {
        let label = self.intern_str(label);
        let func = self.intern_str(func);
        let task = self.intern_str(task);
        let stack = self.intern_stack(stack);
        let delta = self.ts_delta(ts);
        let mut body = Vec::new();
        body.extend_from_slice(&TY_SWITCH.to_le_bytes());
        body.extend_from_slice(&[
            (delta & 0xff) as u8,
            ((delta >> 8) & 0xff) as u8,
            ((delta >> 16) & 0xff) as u8,
        ]);
        write_varint(&mut body, target);
        write_varint(&mut body, label);
        write_varint(&mut body, func);
        write_varint(&mut body, task);
        write_varint(&mut body, stack);
        write_varint(&mut body, thread);
        self.frame(T_EVENT, &body);
    }

    /// Borrow the bytes written so far (for incremental flush by the file writer).
    pub fn bytes(&self) -> &[u8] {
        &self.out
    }

    /// Drop the buffered output bytes, keeping the interning/timestamp state, so a
    /// streaming writer that flushes `bytes()` incrementally doesn't hold the whole
    /// encoded stream in memory. (Kept as part of the streaming-encoder API.)
    #[allow(dead_code)]
    pub fn clear_out(&mut self) {
        self.out.clear();
    }
}

// Field units (carried in schemas; recognized by the viewer per plan R8).
const UNIT_NONE: u8 = 0;
const UNIT_NS: u8 = 1;

#[cfg(test)]
mod tests {
    //! Round-trips the binary format through the Rust encoder → decoder and pins
    //! the on-wire bytes the Python bootstraps must match. Run with `cargo test`.
    use super::*;

    /// Drive a decoder over a full byte stream, collecting every decoded item.
    fn decode_all(bytes: &[u8]) -> Vec<Item> {
        let mut dec = Decoder::new();
        let mut pos = 0usize;
        let mut items = Vec::new();
        loop {
            match dec.step(&bytes[pos..]).expect("decode") {
                Step::NeedMore => break,
                Step::Done { item, consumed } => {
                    pos += consumed;
                    if let Some(it) = item {
                        items.push(it);
                    }
                    if pos >= bytes.len() {
                        break;
                    }
                }
            }
        }
        items
    }

    #[test]
    fn varint_roundtrips() {
        for v in [
            0u64,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            write_varint(&mut out, v);
            let mut pos = 0;
            assert!(matches!(peek_varint(&out, &mut pos), Varint::Val(d) if d == v));
            assert_eq!(pos, out.len());
        }
    }

    #[test]
    fn varint_rejects_overlong_and_signals_need_more() {
        // 11 continuation bytes — past the 64-bit ceiling → Overlong, not a value.
        let overlong = [0x80u8; 11];
        let mut pos = 0;
        assert!(matches!(peek_varint(&overlong, &mut pos), Varint::Overlong));
        // A lone continuation byte (high bit set, buffer ends) → NeedMore.
        let mut pos = 0;
        assert!(matches!(peek_varint(&[0x80], &mut pos), Varint::NeedMore));
    }

    #[test]
    fn zigzag_roundtrips() {
        for v in [0i64, -1, 1, -2, 2, i64::MIN, i64::MAX, -12345, 12345] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn execution_and_gc_roundtrip_through_glr_schemas() {
        let mut enc = Encoder::new();
        enc.write_file_schemas();
        enc.meta(1700, 42, 4321, "{\"runtime\":\"gevent\"}");
        let s1 = Execution {
            gid: 0x55,
            start: 1_000,
            dur: 250,
            name: "Greenlet-3".into(),
            func: "app.py:run:9".into(),
            task: "req-7".into(),
            stack: "app.py:run:9 <- app.py:main:2".into(),
        };
        // A second execution sharing func + stack → must dedupe to the same pool ids.
        let s2 = Execution {
            gid: 0x66,
            start: 2_000,
            dur: 10,
            name: "Hub".into(),
            func: "app.py:run:9".into(),
            task: String::new(),
            stack: "app.py:run:9 <- app.py:main:2".into(),
        };
        enc.execution(&s1);
        enc.execution(&s2);
        enc.gc(&GcEvent {
            start: 500,
            dur: 9,
            generation: 2,
            collected: 7,
        });
        let bytes = enc.bytes().to_vec();

        let items = decode_all(&bytes);
        assert_eq!(items.len(), 4); // meta, execution, execution, gc

        match &items[0] {
            Item::Meta(m) => {
                assert_eq!(m.epoch_ms, 1700);
                assert_eq!(m.tid, 42);
                assert_eq!(m.pid, 4321);
                assert!(m.pyinfo.contains("gevent"));
            }
            _ => panic!("expected meta first"),
        }
        match &items[1] {
            Item::Execution(s) => {
                assert_eq!(s.gid, 0x55);
                assert_eq!(s.start, 1_000);
                assert_eq!(s.dur, 250);
                assert_eq!(s.name, "Greenlet-3");
                assert_eq!(s.func, "app.py:run:9");
                assert_eq!(s.task, "req-7");
                assert_eq!(s.stack, "app.py:run:9 <- app.py:main:2");
            }
            _ => panic!("expected execution"),
        }
        match &items[2] {
            Item::Execution(s) => {
                assert_eq!(s.gid, 0x66);
                assert_eq!(s.start, 2_000); // delta-decoded from base 1_000
                assert_eq!(s.stack, "app.py:run:9 <- app.py:main:2");
            }
            _ => panic!("expected execution"),
        }
        match &items[3] {
            Item::Gc(g) => {
                assert_eq!(g.start, 500);
                assert_eq!(g.generation, 2);
                assert_eq!(g.collected, 7);
            }
            _ => panic!("expected gc"),
        }
    }

    #[test]
    fn switch_event_roundtrips_with_thread_and_ts_reset() {
        // Build a switch schema + two events, the second far enough ahead to force
        // a Ts-reset (gap > 24-bit range).
        let mut enc = Encoder::new();
        enc.schema(
            TY_SWITCH,
            true,
            "switch",
            &[
                ("target", FT_VARINT, UNIT_NONE),
                ("label", FT_PSTR, UNIT_NONE),
                ("func", FT_PSTR, UNIT_NONE),
                ("task", FT_PSTR, UNIT_NONE),
                ("stack", FT_PSTACK, UNIT_NONE),
                ("thread", FT_VARINT, UNIT_NONE),
            ],
        );
        // Helper to emit a switch the same way the bootstrap will.
        let emit = |enc: &mut Encoder, ts: u64, target: u64, label: &str, thread: u64| {
            let l = enc.intern_str(label);
            let delta = enc.ts_delta(ts);
            let mut body = Vec::new();
            body.extend_from_slice(&TY_SWITCH.to_le_bytes());
            body.extend_from_slice(&[
                (delta & 0xff) as u8,
                ((delta >> 8) & 0xff) as u8,
                ((delta >> 16) & 0xff) as u8,
            ]);
            write_varint(&mut body, target);
            write_varint(&mut body, l);
            write_varint(&mut body, 0); // func ""
            write_varint(&mut body, 0); // task ""
            write_varint(&mut body, 0); // stack ∅
            write_varint(&mut body, thread);
            enc.frame(T_EVENT, &body);
        };
        emit(&mut enc, 100, 0xAA, "Greenlet-1", 7);
        emit(&mut enc, 100 + (1 << 25), 0xBB, "Hub", 9); // big gap → reset
        let bytes = enc.bytes().to_vec();

        let items = decode_all(&bytes);
        assert_eq!(items.len(), 2);
        match &items[0] {
            Item::Switch(s) => {
                assert_eq!(s.ts, 100);
                assert_eq!(s.target, 0xAA);
                assert_eq!(s.label, "Greenlet-1");
                assert_eq!(s.thread, 7);
            }
            _ => panic!("expected switch"),
        }
        match &items[1] {
            Item::Switch(s) => {
                assert_eq!(s.ts, 100 + (1 << 25)); // reconstructed across the reset
                assert_eq!(s.label, "Hub");
                assert_eq!(s.thread, 9);
            }
            _ => panic!("expected switch"),
        }
    }

    #[test]
    fn partial_buffer_signals_need_more() {
        let mut enc = Encoder::new();
        enc.write_file_schemas();
        enc.gc(&GcEvent {
            start: 1,
            dur: 2,
            generation: 0,
            collected: 0,
        });
        let bytes = enc.bytes().to_vec();
        // Feed all but the last byte: the final frame must report NeedMore (not
        // error) because its body is one byte short.
        let truncated = &bytes[..bytes.len() - 1];
        let mut dec = Decoder::new();
        let mut pos = 0;
        loop {
            match dec.step(&truncated[pos..]).expect("decode") {
                Step::NeedMore => break, // the last (gc) frame is incomplete → as expected
                Step::Done { consumed, .. } => {
                    pos += consumed;
                    assert!(pos < truncated.len(), "should run short before the end");
                }
            }
        }
    }

    #[test]
    fn rejects_bad_magic() {
        let mut dec = Decoder::new();
        match dec.step(b"NOPE!") {
            Err(e) => assert!(e.to_string().contains("bad magic")),
            Ok(_) => panic!("expected a bad-magic error"),
        }
    }

    #[test]
    fn rejects_oversized_frame_length() {
        // A frame advertising a body far past the cap must error, not buffer toward
        // it (OOM) or overflow when computing the frame end.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.push(T_EVENT);
        write_varint(&mut bytes, MAX_FRAME_LEN + 1);
        let mut dec = Decoder::new();
        let consumed = match dec.step(&bytes).expect("header") {
            Step::Done { consumed, .. } => consumed,
            Step::NeedMore => panic!("header should decode"),
        };
        match dec.step(&bytes[consumed..]) {
            Err(e) => assert!(e.to_string().contains("too large"), "got: {e}"),
            Ok(_) => panic!("expected an oversized-frame error"),
        }
    }

    /// The Python encoder (`src/glr.py`, inlined into the bootstraps) must emit
    /// bytes this decoder reads identically. This drives the real `glr.py` via
    /// `python3` and decodes its output here. Skipped if `python3` isn't present.
    #[test]
    fn python_encoder_matches_rust_decoder() {
        use std::process::Command;
        let script = r#"
import sys, os
sys.path.insert(0, os.environ["GLR_DIR"])
import glr
buf = bytearray()
e = glr.GlrEnc(buf)
e.write_wire_schemas()
e.meta(1700, 42, 99, '{"runtime":"gevent"}')
lid = e.str_id("Greenlet-3")
fid = e.str_id("app.py:run:9")
tid = e.str_id("req-7")
sid = e.stack_id(["app.py:run:9", "app.py:main:2"])
e.switch(1000, 0x55, lid, fid, tid, sid, 7)
# second switch: shares strings/stack (dedup) and jumps far enough to force a reset
e.switch(1000 + (1 << 25), 0x66, e.str_id("Hub"), 0, 0, 0, 9)
e.gc(500, 9, 2, 7)
sys.stdout.buffer.write(bytes(buf))
"#;
        let out = match Command::new("python3")
            .args(["-c", script])
            .env("GLR_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
            .output()
        {
            Ok(o) => o,
            Err(_) => {
                eprintln!("python3 not available — skipping cross-language test");
                return;
            }
        };
        assert!(
            out.status.success(),
            "python encoder failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let items = decode_all(&out.stdout);
        assert_eq!(items.len(), 4, "meta + 2 switches + gc");
        match &items[0] {
            Item::Meta(m) => {
                assert_eq!(m.epoch_ms, 1700);
                assert_eq!(m.tid, 42);
                assert_eq!(m.pid, 99);
                assert!(m.pyinfo.contains("gevent"));
            }
            _ => panic!("expected meta"),
        }
        match &items[1] {
            Item::Switch(s) => {
                assert_eq!(s.ts, 1000);
                assert_eq!(s.target, 0x55);
                assert_eq!(s.label, "Greenlet-3");
                assert_eq!(s.func, "app.py:run:9");
                assert_eq!(s.task, "req-7");
                assert_eq!(s.stack, "app.py:run:9 <- app.py:main:2");
                assert_eq!(s.thread, 7);
            }
            _ => panic!("expected switch"),
        }
        match &items[2] {
            Item::Switch(s) => {
                assert_eq!(s.ts, 1000 + (1 << 25)); // reconstructed across the reset
                assert_eq!(s.label, "Hub");
                assert_eq!(s.func, ""); // id 0
                assert_eq!(s.stack, ""); // id 0
                assert_eq!(s.thread, 9);
            }
            _ => panic!("expected switch"),
        }
        match &items[3] {
            Item::Gc(g) => {
                assert_eq!(g.start, 500);
                assert_eq!(g.dur, 9);
                assert_eq!(g.generation, 2);
                assert_eq!(g.collected, 7);
            }
            _ => panic!("expected gc"),
        }
    }

    /// Materialize a real bootstrap (encoder inlined), run a Python driver that
    /// drives it live, and return everything it streamed over a Unix socket,
    /// decoded by the production decoder. `None` = skip (no python3, didn't connect)
    /// — keeps the E2E tests best-effort and never-blocking on machines without the
    /// right runtime. The driver must end by calling `builtins.__greenlane_teardown`.
    fn run_bootstrap(
        template: &str,
        mode: crate::TraceMode,
        long_ns: u64,
        driver: &str,
    ) -> Option<Vec<Item>> {
        use std::io::Read;
        use std::os::unix::net::UnixListener;
        use std::process::Command;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sock = dir.join(format!(
            "greenlane-wiretest-{}-{}.sock",
            std::process::id(),
            nanos
        ));
        let script = dir.join(format!(
            "greenlane-wiretest-{}-{}.py",
            std::process::id(),
            nanos
        ));
        let _ = std::fs::remove_file(&sock);

        let listener = UnixListener::bind(&sock).ok()?;
        listener.set_nonblocking(true).ok();

        let filled = crate::fill_template(template, &sock, mode, long_ns);
        let cleanup = || {
            let _ = std::fs::remove_file(&sock);
            let _ = std::fs::remove_file(&script);
        };
        if std::fs::write(&script, format!("{filled}\n{driver}")).is_err() {
            cleanup();
            return None;
        }

        let mut child = match Command::new("python3")
            .arg(&script)
            // Quiet on success; the assertions below describe any failure. Flip to
            // Stdio::inherit() to see the target's traceback when debugging.
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                cleanup();
                return None;
            }
        };

        // Accept within a bounded window (python imports + connects on startup).
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break None,
            }
        };
        let mut stream = match stream {
            Some(s) => s,
            None => {
                let _ = child.kill();
                cleanup();
                return None;
            }
        };

        // The accepted socket inherits the listener's non-blocking flag on
        // macOS/BSD, so set_read_timeout alone wouldn't block — read() would
        // return WouldBlock immediately and we'd capture nothing ("got 0 items").
        // Clear it so the read timeout governs, then only stop on real EOF.
        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut bytes = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break, // EOF: the bootstrap process exited.
                Ok(n) => bytes.extend_from_slice(&tmp[..n]),
                // A transient timeout while the driver is still running isn't EOF;
                // keep waiting until the child closes the socket.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = child.wait();
        cleanup();
        Some(decode_all(&bytes))
    }

    fn switches(items: &[Item]) -> Vec<&Switch> {
        items
            .iter()
            .filter_map(|it| match it {
                Item::Switch(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// End-to-end for the gevent bootstrap (needs only `greenlet`, not full gevent):
    /// raw greenlets switching produce decodable switches with the closing
    /// greenlet's yield-point stack in `all` mode.
    #[test]
    fn gevent_bootstrap_streams_decodable_events() {
        let driver = r#"
import greenlet, builtins

main = greenlet.getcurrent()

def worker(rounds):
    for _ in range(rounds):
        main.switch()

g1 = greenlet.greenlet(worker)
g2 = greenlet.greenlet(worker)
for _ in range(6):
    g1.switch(1)
    g2.switch(1)
try:
    builtins.__greenlane_teardown()
except Exception:
    pass
"#;
        let items = match run_bootstrap(crate::BOOTSTRAP, crate::TraceMode::All, 0, driver) {
            Some(i) => i,
            None => return,
        };
        assert!(
            items.iter().any(|it| matches!(it, Item::Meta(_))),
            "expected a meta frame"
        );
        let sw = switches(&items);
        assert!(
            sw.len() >= 2,
            "expected multiple greenlet switches, got {}",
            sw.len()
        );
        assert!(
            sw.iter().all(|s| s.thread != 0),
            "switches carry the OS thread id"
        );
        // `all` mode walks the closing greenlet's gr_frame → some non-empty stacks.
        assert!(
            sw.iter().any(|s| !s.stack.is_empty()),
            "gevent all-mode should capture some yield-point stacks"
        );
    }

    /// The encoder must bound its intern pools so a target under sustained load
    /// with high-cardinality fields can't grow them (and its memory) without limit.
    /// Past the cap, new strings degrade to id 0 ("") and new stacks drop to 0,
    /// while already-interned ids stay resolvable — and the stream still decodes.
    #[test]
    fn python_encoder_caps_intern_pools() {
        use std::process::Command;
        let script = r#"
import sys, os
sys.path.insert(0, os.environ["GLR_DIR"])
import glr
glr.GlrEnc._MAX_POOL = 5  # tiny cap so the test trips it cheaply
buf = bytearray()
e = glr.GlrEnc(buf)
e.write_wire_schemas()
e.meta(1, 1, 1, '{"runtime":"gevent"}')

ids = [e.str_id("s%d" % i) for i in range(50)]
assert max(ids) < glr.GlrEnc._MAX_POOL, "ids must stay under the cap, got %r" % max(ids)
assert ids[-1] == 0, "overflow strings degrade to id 0, got %r" % ids[-1]
assert e._ns <= glr.GlrEnc._MAX_POOL, "string pool stops growing, _ns=%d" % e._ns

sids = [e.stack_id(["f%d" % i, "g%d" % i]) for i in range(50)]
assert sids[-1] == 0, "overflow stacks degrade to id 0, got %r" % sids[-1]
assert e._nk <= glr.GlrEnc._MAX_POOL, "stack pool stops growing, _nk=%d" % e._nk

again = e.str_id("s0")  # already interned: must still resolve even when full
assert again == ids[0] and again != 0, "interned strings stay resolvable, got %r" % again

e.switch(10, 0x1, e.str_id("zzz-overflow"), 0, 0, 0, 1)  # label pool full -> id 0 -> ""
e.switch(20, 0x2, again, 0, 0, 0, 1)                     # label -> "s0"
sys.stdout.buffer.write(bytes(buf))
"#;
        let out = match Command::new("python3")
            .args(["-c", script])
            .env("GLR_DIR", concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
            .output()
        {
            Ok(o) => o,
            Err(_) => {
                eprintln!("python3 not available — skipping cross-language test");
                return;
            }
        };
        assert!(
            out.status.success(),
            "python pool-cap assertions failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let items = decode_all(&out.stdout);
        let sw = switches(&items);
        assert_eq!(sw.len(), 2, "meta + 2 switches");
        assert_eq!(sw[0].label, "", "overflow label decodes as empty string");
        assert_eq!(sw[1].label, "s0", "non-overflow label still decodes");
    }

    /// End-to-end: a correlation id set on the greenlet (`request_id`) is captured
    /// as the execution's `task`, and the cheap leaf-function label (`func`) is
    /// captured on switches — exercising `_task_id`'s __dict__ fast path and the
    /// inlined `_headline_cheap` walk in a real run.
    #[test]
    fn gevent_bootstrap_captures_task_id_and_func() {
        let driver = r#"
import greenlet, builtins

main = greenlet.getcurrent()

def worker(rounds):
    for _ in range(rounds):
        main.switch()

g1 = greenlet.greenlet(worker)
g1.request_id = "req-42"  # instance attr → _task_id reads it from __dict__
for _ in range(4):
    g1.switch(1)
try:
    builtins.__greenlane_teardown()
except Exception:
    pass
"#;
        let items = match run_bootstrap(crate::BOOTSTRAP, crate::TraceMode::All, 0, driver) {
            Some(i) => i,
            None => return,
        };
        let sw = switches(&items);
        assert!(sw.len() >= 2, "expected switches, got {}", sw.len());
        assert!(
            sw.iter().any(|s| s.task == "req-42"),
            "a switch into the greenlet should carry its request_id as task"
        );
        assert!(
            sw.iter().any(|s| !s.func.is_empty()),
            "switches should carry a cheap leaf-function label"
        );
    }
}
