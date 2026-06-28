# greenlane trace format (`.glr` / wire) — specification

> **Status: implemented.** This describes the format greenlane reads and writes
> today (see [ADR 0001](../adr/0001-binary-trace-format.md)). The Rust encoder +
> streaming decoder are in `src/trace_format.rs`; the byte-compatible Python
> encoder is `src/glr.py` (inlined into both bootstraps). The two MUST change
> together — `src/trace_format.rs`'s tests include a cross-language check and an
> end-to-end run of the real bootstrap.

The same byte format is used on the live Unix-socket wire and in the on-disk
`.glr` recording — a recording is the captured stream (with the file storing
closed `slice` events where the wire carries `switch` events). All multi-byte
integers are **little-endian** unless stated. Variable-length integers use
**LEB128** (7 bits/byte, MSB = continuation; a `u64` is ≤10 bytes); signed values
are **zigzag**-encoded then LEB128.

## 1. Stream layout

```text
Stream  := Header Frame*
Header  := Magic(4) Version(1)
Frame   := Tag(1) Len(varint) Body[Len]
```

- `Magic` = `b"GLR\0"`. (The legacy text/postcard `.glr` began with `b"GREENLNE"`;
  greenlane detects it and reports an "unsupported legacy recording" error.)
- `Version` = `u8`, currently `1`. Decoders reject unknown majors.
- **Frames are length-prefixed.** `Len` is the body length as a varint, so the
  streaming reader is trivial (read tag + len, then exactly that many body bytes)
  and an unknown future tag can be **skipped** without understanding it. This
  resolves the original "should frames be length-prefixed?" question in favor of
  forward-skippable frames; the cost is one varint (usually 1 byte) per frame.
- Frames may be appended indefinitely (streaming/append model).

**Ordering constraint:** a schema frame for a given `type_id` **must** appear
before any event frame that references it; a pool entry must be defined before it
is referenced. The encoders guarantee this (they emit a pool/schema frame the
first time a value/type is used, before the event that references it).

## 2. Frame types

| Tag    | Name            | Body                                                       |
| ------ | --------------- | ---------------------------------------------------------- |
| `0x01` | Schema          | `type_id:u16 flags:u8 name:istr nfields:v (fname:istr ftype:u8 unit:u8)*` |
| `0x02` | Event           | `type_id:u16 [ts_delta:u24 if has_timestamp] fields…`      |
| `0x03` | String pool     | `istr` — appends one UTF-8 string; id = next (0 = `""`)     |
| `0x04` | Stack pool      | `n:v (str_id:v)*` — appends one stack; id = next (0 = ∅)    |
| `0x05` | Timestamp reset | `base:u64` — re-establish the absolute ns timestamp base    |
| `0x06` | Meta            | `epoch_ms:v tid:v pid:i64(zigzag) pyinfo:istr`             |

`istr` = inline string = `len:varint utf8[len]`. `v` = varint.

The original `meta`/`tid`/`pyinfo` text lines are collapsed into the single
**Meta** frame (`0x06`). A separate schema-annotation frame (`0x07`) was *not*
needed: field units are carried inline in the Schema frame (see §5).

## 3. Timestamps

- A `has_timestamp` event carries a **24-bit unsigned delta** (3 bytes LE, ns
  since the current base). After decoding it, `base += delta`.
- When an inter-event gap exceeds the 24-bit range (~16.7 ms), the encoder first
  emits a **Timestamp reset** frame (`0x05`) carrying a fresh absolute `u64` base,
  then the event with delta `0`. The initial base is `0` (t0).
- Rationale: small, repetitive deltas stay tiny and compress well.

## 4. Interning pools

The primary size win. Identical values are sent once and referenced by id.

- **String pool** (`0x03`): appends one UTF-8 string per frame; the id is implicit
  (the next index, starting at 1 — id `0` is the predefined empty string). Events
  reference strings by a **varint** id. Used for `label`, `func`, `task`, and the
  individual frames of a stack.
- **Stack pool** (`0x04`): appends one stack per frame as a count + a list of
  **string-pool ids** (one per call frame, leaf → root); the id is implicit (next
  index, starting at 1 — id `0` is the predefined empty stack). The decoder
  reconstructs the joined `" <- "` stack string from the referenced frames. This
  is what makes `--include-traces` affordable: a repeated call stack is one stack
  id plus its frames' string ids, all sent once.
- Referencing an undefined pool id is a stream error.
- Pool id width is **varint** (not fixed `u32`); there is no eviction — a single
  capture's distinct strings/stacks are bounded in practice.

## 5. Schemas & fields

A **Schema** frame (`0x01`) declares:

- `type_id` (`u16`), unique per stream.
- `flags` (`u8`): bit `0x01` = `has_timestamp` (instances carry the 24-bit delta).
- `name` (inline string) — the event name (`switch`, `gc`, `slice`).
- an ordered field list: `(name: istr, type_tag: u8, unit: u8)`.

Field type tags:

| Tag    | Meaning                                  |
| ------ | ---------------------------------------- |
| `0x01` | `Varint` — `u64` LEB128                  |
| `0x02` | `I64` — zigzag LEB128                     |
| `0x03` | `PooledString` — string-pool id (varint) |
| `0x04` | `PooledStack` — stack-pool id (varint)   |
| `0x05` | `Bool` — 1 byte                          |
| `0x06` | `F64` — 8 bytes LE                        |

The high bit `0x80` on a type tag marks the field **optional** (a 1-byte presence
prefix precedes it), reserved for schema evolution; no built-in event uses it yet
(absent strings/stacks use pool id `0` instead). Units are inline per field:
`0` none, `1` ns, `2` us, `3` ms, `4` s, `5` bytes; the viewer formats by unit
(plan R8). A decoder can **skip an unknown event type** generically by reading
each field according to its schema's type tags.

## 6. Built-in event schemas

- **`switch`** (`type_id 1`, `has_timestamp`): `target: Varint`,
  `label: PooledString`, `func: PooledString`, `task: PooledString`,
  `stack: PooledStack`, `thread: Varint`. The event `ts` is the resume time; the
  collector closes the previous interval **on the same `thread`** and opens this
  one. (`origin` from the old text line is dropped — it was never used. A `throw`
  is encoded as a `switch`; the distinction was unused downstream. `thread` lets
  concurrent runtime threads share one socket without truncating each other.)
- **`gc`** (`type_id 2`): `start: Varint{ns}`, `dur: Varint{ns}`,
  `generation: I64`, `collected: I64`.
- **`slice`** (`type_id 3`, `has_timestamp`): `dur: Varint{ns}`, `gid: Varint`,
  `name: PooledString`, `func: PooledString`, `task: PooledString`,
  `stack: PooledStack`. The event `ts` is the slice `start`. This is the
  **file-only** representation of a closed interval — the wire never carries it.

Schemas reserved for planned work (defined when those records land):

- **`sched_switch`** — on/off-CPU transitions (plan R2).
- **`cpu_sample`** — sampled callchain, `stack: PooledStack` (plan R3).
- **`dropped`** — `count: Varint`, load-shed marker (plan R4).
- **`rusage` / `accept_queue`** — extra metric sources (plan R6).

## 7. Versioning & compatibility

- Additive changes (new frame tags, new optional fields, new `type_id`s) do **not**
  bump the major version: frames are length-prefixed so an old decoder skips
  unknown tags, and it can skip unknown event types field-by-field via the schema.
- Breaking changes bump the major in `Header.Version`; decoders reject unknown
  majors.
- Legacy **text/postcard** `.glr` files predate this format and are rejected by
  magic with a clear "re-record" message.

## 8. Resolved decisions (was: open questions)

- **Magic / tags:** `b"GLR\0"`; tag numbering as in §2.
- **Length-prefixing:** yes — every frame is `Tag Len Body` (enables forward-skip).
- **Stack representation:** pooled `file:qualname:line` **strings** captured in
  Python (a stack = a list of string-pool ids). Raw-address symbolization is left
  for a future event type if needed.
- **Pool id width:** varint; no capacity bound / eviction.
- **Meta:** one `Meta` frame (epoch + tid + pid + pyinfo), not separate lines.
- **Compression:** none yet — the delta-timestamp + pool design is built to
  compress; per-segment compression is plan R7.
- **Portability:** fixed little-endian, pure bytes, no OS dependency — safe across
  Linux and macOS (and architectures).
