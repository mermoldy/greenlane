# ADR 0001 — Move off the tab-delimited trace format

- **Status:** accepted (implemented 2026-06-28)
- **Date:** 2026-06-28
- **Deciders:** greenlane maintainers
- **Related:** [implementation plan R1](../design/implementation-plan.md#r1--binary-wire--glr-format-with-interning-pools),
  [glr-format spec](../design/glr-format.md)

> **Implementation:** the format is implemented and is the only one greenlane
> reads/writes. The Rust encoder + streaming decoder live in
> [`src/trace_format.rs`]; the byte-compatible Python encoder is
> [`src/glr.py`], inlined into both bootstraps at materialization. The collector
> ([`src/main.rs`]) and the `.glr` reader/writer ([`src/record.rs`]) both run the
> decoder/encoder. Resolved spec TBDs are recorded in
> [`docs/design/glr-format.md`]; the notable ones: frames **are** length-prefixed
> (`Tag Len Body`) so unknown tags are skippable; pool ids are varints; stacks
> pool a list of frame **string** ids; the `switch` event drops the unused
> `origin` and adds the per-thread `thread` id; the `.glr` stores closed `slice`
> events (the wire carries `switch` events the collector closes).

## Context

The target bootstrap streams events to greenlane as **tab-delimited UTF-8 text**
over a Unix STREAM socket, and the `.glr` recording is that same stream flushed to
disk. The current per-switch line (`src/bootstrap_gevent.py`) is:

```text
t_ns \t event \t origin \t target \t label \t func \t task \t stack \n
```

with side-channel lines `meta\t<epoch_ms>`, `tid\t<n>`, `pyinfo\t<json>`, and
`gc\t<start>\t<dur>\t<gen>\t<collected>`. The Rust side splits on tabs in
`parse_event` / `parse_gc` (`src/main.rs`).

This was the right call to get moving, but it has three structural problems:

1. **The stack field dominates the wire and memory.** The architecture doc already
   notes call traces are "by far the largest field on the wire and in memory,"
   which is why `--include-traces` is off by default. The same stack is re-sent in
   full on nearly every switch even though it barely changes — there is no
   deduplication.
2. **Hot-path cost.** Formatting decimal integers and escaping/encoding text runs
   on the target's hub thread on every greenlet switch (a path that hits ~1M
   switches/s in the stress app). Text is more work than writing packed bytes.
3. **No schema or versioning.** The `.glr` format cannot evolve without silently
   breaking old recordings, and there is no room for new event types (off-CPU
   intervals, CPU samples, extra metrics) without ad-hoc new line prefixes.

## Decision

Replace the text stream with a **framed binary format** (full byte-level spec in
[`docs/design/glr-format.md`](../design/glr-format.md)). The same format is used
on the wire and in the `.glr` file. Key properties:

- **Self-describing frames** behind a `Header(magic, version)`; a 1-byte tag per
  frame; **schema frames** declare event layouts and must precede the events that
  reference them.
- **Interning pools** for strings and stacks: identical labels/stacks are sent
  once and referenced by a compact id thereafter. This is the primary size win and
  the thing that makes traces cheap enough to leave on.
- **Delta-encoded timestamps** with a forward-marching base + reset frames.
- **LEB128 varints**, little-endian throughout.
- **Field unit annotations** (`ns/us/ms/s/bytes`) carried in the schema for the
  viewer.

The collector still emits the same `Slice` / `GcEvent` to everything downstream,
so the DB, viewer, and `greenlane open` path are unaffected by the encoding
change.

## Consequences

### Positive

- Large reduction in wire volume and `.glr` size, concentrated on the stack field.
- Lower hot-path overhead on the target's hub thread.
- Traces become affordable enough to consider sampling-on-by-default (plan R4).
- A schema'd, versioned format unlocks new event types (R2 off-CPU, R3 CPU
  samples, R6 extra sources) without format hacks, and compressible segments (R7).

### Negative / costs

- An encoder (Python, hot-path-sensitive) and a decoder (Rust) to build and test.
- **Backwards incompatibility:** existing text `.glr` files won't load. Mitigation:
  gate on the header magic and emit a clear error; optionally keep a one-version
  text reader behind a flag for a transition period.
- More upfront complexity than appending text lines.

## Alternatives considered

- **Keep text, just gzip the socket/file.** Cuts file size but not hot-path
  formatting cost, still re-encodes the full stack each event before compression,
  and adds nothing toward schema evolution or new event types.
- **JSON / MessagePack per event.** Self-describing but still re-sends repeated
  stacks/labels in full (no interning) and carries per-event key overhead; worse
  on the hot path than packed binary.
- **Columnar on the wire.** Best compression but needs whole-column buffering,
  which fights the streaming/append model greenlane relies on for live view.

## Notes

- The binary encoder on the target must stay allocation-free on the hot path
  (preallocated buffers, pool lookups by id).
- Format is pure bytes with no OS dependency — safe across Linux and macOS.
