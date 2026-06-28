# greenlane — implementation plan

The bridge that makes in-process instrumentation techniques available to
greenlane: **the injected bootstrap runs _inside_ the target process**, so
in-process tricks (binary encoding on the hot path, `perf` fds, in-process
sampling) are available to the bootstrap — greenlane just bootstraps them
remotely (via `sys.remote_exec`, PEP 768) instead of at compile time.

This document is the implementation plan for each idea. Items are ordered by
leverage; later items depend on earlier ones where noted.

Status legend: `proposed` (not started) · `in progress` · `done`.

---

## R1 — Binary wire / `.glr` format with interning pools

**Status:** done · **Priority:** P0 (spine; unlocks R3, R4, R7) · **Effort:** L

> Implemented: `src/trace_format.rs` (Rust encoder + streaming decoder),
> `src/glr.py` (byte-compatible Python encoder, inlined into both bootstraps),
> with the collector and `.glr` reader/writer ported over. Spec + resolved TBDs:
> [glr-format.md](glr-format.md); rationale: [ADR 0001](../adr/0001-binary-trace-format.md).

### Motivation

greenlane currently streams **tab-delimited text** over the Unix socket, with the
full call-stack string repeated on every event. The architecture doc itself notes
traces are "by far the largest field on the wire and in memory," which is why
`--include-traces` is off by default. Text framing also makes the `.glr` file
large and bars schema evolution.

### What greenlane had (pre-R1, for context)

- `src/bootstrap_gevent.py` wrote `t_ns \t event \t origin \t target \t label \t
  func \t task \t stack` lines to the socket.
- `src/main.rs::parse_event` / `parse_gc` split on tabs.
- `.glr` was the same stream flushed to disk (`src/record.rs`, `src/db.rs`).

(All replaced by the binary format — see the **Implemented** note above.)

### Proposed approach

- **Framed binary stream**: `Header(magic, version) | Frame | Frame | …`; 1-byte
  frame tag; schema frames must precede events that reference them.
- **Schema frames + `type_id` + versioning** so the `.glr` format can evolve
  without breaking old recordings.
- **String pool + stack pool frames**: dedupe identical labels/stacks to varint
  pool IDs (the primary size win for high-frequency sampling). This is what makes
  traces affordable enough to leave on. _(As built: varint ids, not fixed 4-byte;
  a stack pools a list of frame string-ids — see [glr-format.md](glr-format.md).)_
- **24-bit delta timestamps** with a forward-marching base + reset frames, instead
  of absolute `t_ns` per event.
- **LEB128 varints** for gids/targets; little-endian throughout.
- **Field unit annotations** (`ns/us/ms/s/bytes`) carried in the schema so the
  viewer formats consistently (see R8).

### Steps

1. Write the spec as `docs/design/glr-format.md` (first ADR candidate — see R10).
2. New crate-internal module `src/trace_format.rs` (or a small split crate):
   encoder + decoder, pools, varint/LEB128, delta timestamps.
3. Port the bootstrap writer: emit binary frames from `bootstrap_gevent.py`
   (and the asyncio one) instead of tab lines. Keep the `_socket` raw-C write
   path; only the payload bytes change.
4. Replace `parse_event` / `parse_gc` in `main.rs` with the binary decoder; the
   collector still produces the same `Slice`/`GcEvent`, so downstream is
   untouched.
5. Update `record.rs` so `.glr` is the framed stream (optionally compressed — see
   R7).
6. Bump the `.glr` version; add a decode test fixture.

### Notes / risks

- Keep the bootstrap encoder allocation-free on the hot path (preallocated
  buffers, pool lookups by id).
- Cross-platform: pure bytes, no OS dependency — safe on Linux + macOS.
- Migration: old text `.glr` files won't load; gate by header magic and print a
  clear error (or keep a one-version text reader behind a flag).

---

## R2 — Off-CPU / preemption visibility

**Status:** proposed · **Priority:** P0 (closes greenlane's core blind spot) ·
**Effort:** M (Linux), L (macOS)

### Motivation

`greenlet.settrace` only fires on **cooperative** switches. When the hub blocks in
a C extension, a syscall, or is preempted by the OS, greenlane draws a long span
but **cannot distinguish "ran 50 ms on-CPU" from "held the hub but was off-CPU
48 ms."** That is exactly the ambiguity the slow-log surfaces.

### What greenlane has today

- The bootstrap reports the hub/loop thread id (in the binary `meta` frame), and
  every `switch` event now carries the OS `thread` id it ran on (R1).
- `src/sysinfo.rs` already samples kernel scheduler lag for that tid.

### Proposed approach

- From the bootstrap (in-process), open a per-thread `perf_event_open` fd on the
  hub tid with `PERF_COUNT_SW_CONTEXT_SWITCHES` (one fd per worker thread).
- Emit context-switch events (on/off-CPU transitions) as a new schema'd event
  type (R1).
- In the viewer, **shade each span on-CPU vs off-CPU** so a long span reads as
  "blocked off-CPU" vs "burning CPU."

### Steps

1. Add a perf-fd helper to the bootstrap (Linux), guarded behind a capability
   check; degrade silently if `perf_event_open` is denied.
2. Define the `sched_switch` event in the trace schema (R1).
3. Collector: fold on/off-CPU intervals alongside slices.
4. Viewer: span shading + a legend; surface "% off-CPU" in the slow-log row.

### Notes / risks

- **Linux-first.** macOS has no `perf`; an equivalent needs `thread_info` /
  `proc_pid_rusage` or kdebug — track as a separate, lower-priority sub-task and
  gate by `cfg!(target_os)` / runtime detection.
- Requires the target to allow perf (paranoid sysctl / capabilities). Reuse the
  same permission-diagnostics pattern as inject failures in `main.rs`.

---

## R3 — In-span CPU sampling profiler (flamegraph per span)

**Status:** proposed · **Priority:** P1 · **Effort:** M · **Depends on:** R1

### Motivation

A 200 ms CPU-bound span is a black box today: you get the leaf function at the
switch boundary, not where the time went _within_ it.

### Proposed approach

- Bootstrap runs a process-wide sampling profiler capturing callchains at a
  configurable frequency (default 99 Hz), tagged with tid + timestamp.
- Stacks dedupe into the **stack pool** (R1), so sampling is cheap on the wire.
- Viewer: for a selected span, build a flamegraph from samples whose timestamp
  falls in `[span.start, span.end)`.

### Steps

1. Perf sampler in the bootstrap (Linux), frequency knob `--sample-hz`.
2. `cpu_sample` event type in the schema (R1), referencing pooled stacks.
3. Collector stores samples in a queryable column (reuse the DB).
4. Viewer: per-span flamegraph panel; link from the slow-log.

### Notes / risks

- Linux-first (same `perf` constraint as R2).
- Symbolization deferred to the analysis/viewer layer (raw addresses on the wire;
  resolve later).

---

## R4 — Poisson sampling + hot-path rate limiting

**Status:** proposed · **Priority:** P1 · **Effort:** S · **Depends on:** R1 (for
"dropped" accounting in the format)

### Motivation

`--include-traces` now has an `off`/`slow`/`all` gate (default `slow` walks only
spans over the warn threshold), so full stacks are no longer all-or-nothing. R4
goes further along the same axis: a **statistical** sample of stacks (capture
~1/N) as a middle ground between "slow only" and "all", plus load-shedding. Under
a switch storm (the stress app hits ~1M switches/s) the bootstrap's blocking
socket writes can backpressure the very hub being measured, and greenlane has no
load-shedding today.

### Proposed approach

- **Poisson sampling primitive**: a lightweight PRNG (e.g. SplitMix64) +
  `draw_exponential(mean)` (shifts/multiplies, no allocation). Use it to capture
  full stacks on a statistically representative ~1/N of switches — a middle ground
  between "leaf only" and "every switch."
- **Hot-path rate limiter** (token bucket): when the event rate exceeds a budget,
  **shed and record that you dropped** rather than block the hub. Emit a
  `dropped(count)` marker so the viewer can show gaps honestly.

### Steps

1. Port the sampling primitive into the bootstrap (Python) and/or keep
   trace-capture decisions there.
2. Add `--trace-sample` (rate) alongside `--include-traces`.
3. Token-bucket limiter around socket writes; `dropped` event type (R1).
4. Viewer: render dropped regions; show drop count in the header.

### Notes

- Cross-platform; pure logic.
- "Record what you dropped" is the rule — silent truncation must never look like
  full coverage.

---

## R5 — Headless analysis path (`greenlane analyze`)

**Status:** proposed · **Priority:** P1 · **Effort:** M

### Motivation

greenlane is browser-only despite living in a Claude-Code repo full of skills. A
headless path that tells you _what_ is wrong (not just _that_ something is) makes
it usable in CI and triage.

### Proposed approach

- `greenlane analyze <file.glr>` subcommand emitting text/JSON: top stalls, GC
  pressure, lane imbalance, off-CPU hotspots (once R2 lands), slowest stacks.
- Optionally a repo skill that wraps it for agent use.

### Steps

1. New `Cmd::Analyze` in `main.rs` reusing the `.glr` reader (`record.rs`).
2. A handful of queries over the slice store (`db.rs`).
3. `--format text|json`.
4. Optional `.claude` skill wrapper.

### Notes

- No new platform deps; reuses existing reader and DB.

---

## R6 — More cheap data sources on the same timeline

**Status:** proposed · **Priority:** P2 · **Effort:** S each · **Depends on:** R1

Cheap, on-theme additions:

- **`getrusage()` voluntary/involuntary context switches** — corroborates the
  off-CPU story (R2); greenlane already samples `sysinfo`.
- **Linux socket accept-queue depth** — a classic gevent-server stall cause;
  render as its own lane.

Each is a new schema'd event type (R1); no collector changes (downstream already
treats events as generic intervals/metrics).

---

## R7 — Compressed, sealed `.glr` segments

**Status:** proposed · **Priority:** P2 · **Effort:** M · **Depends on:** R1

### Motivation

`.glr` is one file today, rewritten on each periodic partial flush. Those flushes
are now **atomic** (write a temp, fsync, rename — `src/record.rs`), so a mid-write
crash no longer truncates the last good recording. Still missing: **sealed,
compressed segments** streamed to disk, which are far smaller and let a hard kill
keep everything up to the last sealed segment instead of rewriting the whole file.

### Proposed approach

- Stream the framed binary (R1) into sealed segments; compress on seal (the
  delta-timestamp + pool design is built to compress).
- Pluggable sink abstraction (local dir now; remote later).

---

## R8 — Field unit annotations in the viewer

**Status:** proposed · **Priority:** P3 · **Effort:** XS · **Depends on:** R1

Carry `unit` (`ns/us/ms/s/bytes`) in schema annotations so the viewer formats
durations/sizes consistently instead of hardcoding. Trivial once schemas exist.

---

## R9 — asyncio support wired into `attach`

**Status:** done · **Priority:** P2 · **Effort:** M

`attach --runtime <gevent|asyncio|auto>` selects the bootstrap; `auto` (the
default) inspects the target's loaded modules at attach time and injects gevent if
the `gevent` package is imported, otherwise asyncio. asyncio uses `sys.monitoring`
(PEP 669) on coroutine resume/suspend; both stream the same wire protocol so the
viewer is identical.

---

## R10 — Repo conventions: design docs + ADRs

**Status:** done · **Priority:** P3 · **Effort:** XS

Adopted the `docs/design/` + `docs/adr/` split. This plan + the format spec live
under `docs/design/`; [ADR 0001](../adr/0001-binary-trace-format.md) ("why we are
leaving the tab-delimited format", R1) is the first ADR.

---

## R11 — Hub grouping with collapse-to-single-lane

**Status:** proposed · **Priority:** P1 · **Effort:** M

> Groundwork landed (R1): every `switch` event already carries the OS `thread` id
> it ran on, and the collector keeps per-thread state. In the single-hub case that
> thread id _is_ the hub, so step 1's "owner id per slice" needs no further wire
> work — what remains is propagating it through the store/query and the viewer
> grouping. (The id is per OS thread, not yet a logical hub id for multi-hub apps.)

### Motivation

A real app spawns many greenlets/tasks, and today each gets its own lane — the
timeline becomes a wall of sparse rows that's hard to read at a glance. But every
greenlet belongs to exactly one **hub** (the gevent Hub / asyncio event loop on a
single OS thread), and cooperative scheduling guarantees **at most one greenlet
per hub runs at any instant**. That temporal exclusivity means a hub's greenlets
can share one row losslessly — no two spans overlap (and if they do, that's R12).

### What greenlane has today

- One lane per greenlet/task; the Hub maps to the scheduler lane
  (`docs/architecture.md`).
- Lanes are orderable by identity or recent activity, but not grouped.

### Proposed approach

- Model a **hub** as a grouping over lanes: associate each greenlet/task with its
  owning hub (greenlane already knows the hub/loop tid). Multi-hub topologies
  (multiple gevent hubs, thread-per-core asyncio) yield multiple groups.
- Viewer: a **collapsible group header** per hub.
  - **Expanded:** today's behavior — one lane per greenlet under the header.
  - **Collapsed:** a **single merged lane** rendering every span from that hub's
    greenlets on one row, colored per greenlet (or per state). Since spans are
    temporally exclusive within a hub, this reads cleanly as "what this hub was
    doing over time."
- Make collapsed the natural default above a lane-count threshold (a coarse LOD
  knob, complementary to R1's server-side LOD seam).

### Steps

1. Carry a `hub` / owner id per slice (add to the `switch` schema — R1; in the
   single-hub case it's the one known hub tid and needs no wire change).
2. Collector/store: group lanes by hub; expose grouping + collapsed-merge in the
   window/query API (`db.rs`, `server.rs`).
3. Viewer: group headers, collapse/expand toggle, and a single-lane renderer that
   merges a group's spans onto one row.

### Notes

- Cross-platform; pure viewer + grouping logic.
- The collapsed lane is the right surface to overlay R12 overlap markers and R2
  on/off-CPU shading.

---

## R12 — Scheduler-invariant error detection (overlap)

**Status:** proposed · **Priority:** P1 · **Effort:** M · **Depends on:** R11
(scope), pairs with R5 (headless reporting)

> Groundwork landed: the collector already tracks the currently-running unit
> **per thread** (the fold path's per-thread `cur` map), which is exactly the
> per-hub running-unit tracker step 1 needs (thread == hub in the single-hub case).
> The DB also no longer _corrupts_ on overlap — `window()` falls back to an overlap
> scan when spans are non-monotonic (`sorted` flag, `src/db.rs`) — so overlap is
> rendered correctly; what's missing is _detecting + reporting_ it as an anomaly.

### Motivation

In a cooperative single-threaded scheduler, exactly one unit runs at a time **per
hub**. If two slices on the same hub **overlap in time**, an invariant is broken —
which means one of: a bug in greenlane's slice folding; a timestamp/clock anomaly
(deltas going backwards); a malformed event ordering; or a genuine concurrency
surprise (a C extension releasing the GIL and running Python, a stray thread, two
hubs misattributed to one). Today these corrupt the picture silently. Surfacing
them turns a subtle data-integrity problem into a visible, actionable signal.

### Conditions to detect

- **Same-hub span overlap** — the core cooperative invariant (most important).
- A greenlet span overlapping its own hub's scheduler span.
- **Time going backwards** — a switch timestamp earlier than the previous (delta
  underflow); a clock or ordering error.
- **State-machine inconsistency** — switch _into_ a unit already marked running,
  or _out of_ one not running.

### Proposed approach

- Validate in the collector as slices are folded: track the currently-running unit
  per hub; when a new switch-in implies an overlap (or the ordering is impossible),
  emit an `overlap` / `anomaly` record with the two offending slice ids and the
  overlap window.
- Make anomalies first-class and queryable, like the slow-log.
- Viewer: tint overlap regions with a distinct **error** color and a marker; an
  **errors panel/badge** (sibling to the slow-log) listing anomalies with
  click-to-seek. Header carries the count.
- `greenlane analyze` (R5) reports anomalies headlessly for CI.

### Steps

1. Collector invariant checker (per-hub running-unit tracker) producing anomaly
   records (`main.rs` fold path).
2. Store + query for anomalies (`db.rs`).
3. Viewer error markers + panel + header badge.
4. Wire into R5's text/JSON output.

### Notes

- The overlap scope is "same hub" — so this builds directly on R11's hub model.
- Distinguish **greenlane bugs** (fold/parse) from **target anomalies** (real GIL
  release / threads) in the anomaly label; the first is something to fix, the
  second is a genuine finding worth showing the user.

---

## R13 — Kernel scheduler-lag graph on the timeline

**Status:** proposed · **Priority:** P1 · **Effort:** S

### Motivation

greenlane already samples the kernel **run-queue delay** (scheduler lag) for the
hub thread, but only exposes it as a live number in the System panel. Plotting it
as a time-aligned band — exactly like the CPU graph — shows _when_ the OS starved
the hub of CPU, which directly explains spans that look long but were really
**off-CPU waiting to be scheduled** (the complement to R2). It answers "was this
stall my code, or the machine being oversubscribed?"

### What greenlane has today

- `src/sysinfo.rs` samples scheduler lag for the hub tid (System panel only).
- The viewer already renders a **CPU graph** band above the lanes and **GC**
  vertical markers — the rendering machinery to reuse is in place.

### Proposed approach

- Time-bucket the lag samples and render a **graph band above the lanes**,
  alongside (stacked with / toggleable against) the CPU graph, on the same time
  axis.
- Carry lag samples as a schema'd metric (R1) so they live in the store and
  recordings, instead of being a live-only sysinfo readout.
- Hover shows the sampled lag; optionally tint the band past a configurable
  threshold, mirroring the warn/block treatment of spans.

### Steps

1. Emit timestamped lag samples into the store (extend the sysinfo channel; new
   `sched_lag` metric type — R1).
2. Viewer: a lag graph band reusing the CPU-graph renderer; legend + hover.
3. Persist in `.glr` so recordings show it too.

### Notes

- Cross-platform: lag sampling already exists where supported; degrade to an empty
  band where it isn't.
- Reads best next to R2 (off-CPU shading) — together they separate "off-CPU,
  runnable but not scheduled" (lag) from "off-CPU, blocked" (R2).

---

## R14 — All threads & thread pools on the timeline (hub is one thread)

**Status:** proposed · **Priority:** P1 · **Effort:** L · **Depends on:** R11
(grouping), leverages R2/R3 (per-thread perf)

> Groundwork landed: each `switch` event carries its OS `thread` id (R1), the seed
> for "thread as top-level grouping." Still TODO from step 1: tagging **GC events**
> with the collecting thread's tid (GC frames currently carry no thread, so markers
> stay global) and enumerating thread/threadpool metadata.

### Motivation

A gevent/asyncio app is not just its hub thread. Real processes also run OS
threads, the **gevent threadpool** (for blocking calls), `concurrent.futures`
executors, and native threads inside C extensions. greenlane is hub-centric today
and shows only the scheduler thread, so anything happening off the hub —
threadpool saturation, GIL contention, a background thread hogging the CPU — is
invisible. Promoting **every thread and threadpool to a first-class timeline row**,
with the **hub nested as one thread among them**, gives the whole-process picture.

### What greenlane has today

- A single hub/loop thread is the focus; the CPU graph tracks "the single
  scheduler thread."
- GC markers are **global vertical lines**, not attributed to a thread.
- `sysinfo` knows the host/process and can enumerate threads.

### Proposed approach

- **Thread becomes the top-level grouping.** Hierarchy: `Thread → (Hub →)
  greenlets/tasks`. The hub is simply the thread that runs the event loop; R11's
  hub grouping nests inside its owning thread. Thread pools render as a group of
  worker-thread rows (gevent threadpool, `ThreadPoolExecutor`).
- **Per-thread GC attribution.** GC runs on whichever thread holds the GIL when
  the threshold trips. Capture `threading.get_native_id()` in the `gc.callbacks`
  handler and tag each GC event with that tid, so the marker lands on the right
  thread's row instead of a global line.
- **Activity for non-hub threads.** The cooperative trace hook only sees the hub,
  so other threads' activity comes from **sampling** — reuse R2/R3's per-thread
  `perf` fds (on/off-CPU + CPU samples per tid) to draw what each thread is doing.
  Where perf is unavailable, fall back to coarse periodic thread-state sampling.
- **A CPU graph per thread.** Today's single CPU-usage band tracks only the
  scheduler thread; here **each thread row carries its own CPU graph** (busy
  fraction of that tid), plus its own kernel-lag band (R13). The global band
  becomes the per-thread band repeated per row — so you can see one thread pegged
  while the hub idles, threadpool workers saturating, etc.

### Steps

1. Bootstrap: enumerate threads (`threading.enumerate()` + native tids) and report
   thread metadata; tag GC events with the collecting thread's tid.
2. Instrument/observe thread pools where feasible (gevent threadpool,
   `concurrent.futures`) to label worker rows and queue depth.
3. Per-thread activity via R2/R3 perf fds (one per tid), or sampled fallback.
4. Collector/store: thread as top grouping; nest hub + greenlets; per-thread GC
   and metrics (`db.rs`, `server.rs`).
5. Viewer: thread rows + threadpool group, per-thread GC markers, and a
   **per-thread CPU graph + lag band** on each row (the global CPU band becomes
   per-thread); collapse/expand consistent with R11.

### Notes / risks

- Biggest lift here is **seeing threads the cooperative hook can't** — sampling
  (R2/R3/R4) is the mechanism; without it, non-hub rows show only GC + coarse
  state.
- Threadpool instrumentation is runtime-specific (gevent threadpool vs asyncio
  `run_in_executor`); land gevent first.
- Cross-platform thread enumeration is fine; rich per-thread perf is Linux-first.

---

## R15 — GC overlay toggle (button)

**Status:** done · **Priority:** P3 · **Effort:** XS · **Viewer-only**

> Implemented: a **GC on/off** button in the viewer's bottom bar (beside _system
> info_) toggles `Timeline.showGc`, which gates both the GC marker draw and the GC
> hover readout. The data is untouched — still collected, queried, and counted in
> the header "GC" stat. The choice persists across sessions in `localStorage`
> (`gl.showGc`) and is applied to the timeline on load.

### Motivation

GC pauses are drawn as vertical markers across the grid. On a GC-heavy capture
they clutter the timeline and obscure the spans underneath. A simple toggle lets
you hide them when you want a clean view and bring them back when investigating GC
pressure.

### What greenlane has today

- GC pauses render as **always-on** vertical markers across the lanes (with
  generation/duration/objects on hover).

### Proposed approach

- A **GC button** in the viewer toolbar (next to the existing controls / **system**
  button) that toggles the GC marker layer on the grid on/off.
- Reflect state in the button (active/inactive) and persist the choice across the
  session (and ideally in the URL/view state, like other viewer toggles).
- Pure render toggle — the GC data is still collected, queried, and counted in the
  header; only its drawing is suppressed.

### Steps

1. Add a toggle to the toolbar and a `showGc` flag in the viewer's view state.
2. Gate the GC marker layer render on the flag.
3. Persist with the other view toggles (lane order, time axis, etc.).

### Notes

- Trivial, self-contained, no wire/collector changes.
- When R14 lands (per-thread GC markers), the same toggle hides them on every
  thread row; consider a later refinement to toggle per generation.

---

## Suggested sequencing

```text
R1 (binary format) ──┬─> R3 (in-span flamegraph)
                     ├─> R4 (sampling + rate limit)
                     ├─> R6 (extra sources)
                     ├─> R7 (compressed segments)
                     ├─> R8 (units)
                     └─> R11 (hub grouping; single-hub case needs no wire change)
R2  (off-CPU) ───────┬> (depends on R1 schema; Linux-first)
                     └> R14 (per-thread activity via perf)
R3  (CPU samples) ────> R14 (per-thread samples)
R11 (hub grouping) ──┬> R12 (overlap detection; same-hub scope)
                     └> R14 (thread → hub → greenlets hierarchy)
R13 (kernel-lag graph) ─ small; extends the CPU-graph band; pairs with R2
R5  (headless analyze) ─ independent, reuses existing reader; consumes R12 anomalies
R15 (GC overlay toggle) ─ independent; viewer-only, no deps
R9  (asyncio wiring) ── done
```

The spine is **R1 → R2/R3**: R1 makes rich data cheap to move and store; R2/R3 are
the "tell me _what's_ wrong" capabilities that close greenlane's cooperative-only
blind spot. **R11 → R12** is a parallel readability+correctness track: hub grouping
makes dense captures legible, and overlap detection turns the cooperative
invariant it relies on into an error signal. **R13/R14** widen the lens from the
single hub to the whole process — kernel lag, every thread, and per-thread GC —
which leans on the per-thread `perf` machinery from R2/R3 and the grouping from R11.
