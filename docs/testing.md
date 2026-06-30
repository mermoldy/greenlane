# greenlane — testing

This document covers how to exercise greenlane: the demo app that generates a
realistic workload to attach to, and the automated checks. For how greenlane
works internally, see [architecture.md](architecture.md); for installation and
usage, see the [README](../README.md).

## Demo / load generator

A standalone script under `bench/` produces a live target you can attach
greenlane to:

- `bench/app.py` — a pool of greenlets (exercises the gevent bootstrap).

It drives a randomised mix designed to look like a real service rather than a
synthetic spinner: the overwhelming majority of jobs are fast cooperative I/O,
roughly one in ten is a heavier CPU-bound burst (which shows up as a fat execution,
often crossing the highlight thresholds), and about one in a hundred is a very
slow request. That long tail is what makes the slow-log and the execution highlights
worth looking at, so the timeline always has something interesting on it.

It takes these knobs:

- `-c, --concurrency N` — how many greenlets are alive at once (default `50`).
  This is the number of greenlets you'll see.
- `-n, --events N` — total number of jobs to run, or `0` to run forever
  (default `0`).
- `--seed N` — RNG seed for a reproducible run (defaults to the PID).

```sh
# Modest, runs forever — good for a first look:
python bench/app.py

# Heavy concurrency to stress the collector and the viewer:
python bench/app.py -c 5000

# A bounded run that stops after 100k jobs:
python bench/app.py -c 200 -n 100000
```

The script logs its PID and a once-a-second rate line (greenlet switches per
second), so you can see the load it is generating. Logging uses `structlog` when
it is installed and falls back to a
plain timestamped line otherwise, so the script runs with nothing beyond
`gevent` installed.

To watch one under greenlane, start it, note the printed PID, and attach:

```sh
python bench/app.py -c 500 &
greenlane attach <PID> --serve
```

Push `-c` high to find the limits of the hot-path hook, the streaming layer, and
the WebGL renderer.

## Automated checks

**Python** — `tests/test_glr.py` round-trips the binary trace encoder
(`src/glr.py`) through a reference decoder defined in the test, covering varint/
zigzag encoding, string and stack interning/dedup, the schema and meta frames, and
switch/gc events including the 24-bit timestamp delta and its reset on a large gap.
`tests/test_bootstrap.py` materializes `src/bootstrap.py` the way the Rust injector
does — inlining `src/glr.py` and substituting the `__TRACE_MODE__` / `__LONG_NS__` /
`__SOCKET_PATH__` placeholders — and compiles the result at every trace mode, so a
broken template or encoder is caught without a live target. (The bootstraps target
CPython 3.14+, so a 3.14+ interpreter is required.) Use the pinned tools via `uv`
(`uv run` installs the dev group, including `pytest` and `structlog`, on first use):

```sh
uv sync --dev      # one-time: install the dev dependency group
uv run pytest
```

**Rust** — `cargo test` runs the core unit tests (in-crate `#[cfg(test)]`
modules, since the binary has no library target):

```sh
cargo test
```

These cover the data layer and the wire contracts the rest of the system leans
on: the DB query path (`db.rs` — viewport windowing including overlapping/
out-of-order executions, the slow-log threshold/tier filter, duration percentiles, GC
passthrough) driven end-to-end through the public `Db` handle; the `.glr`
recording round-trip (`record.rs` — write → read, including legacy/bad-magic
rejection); the binary trace format (`trace_format.rs` — varint/zigzag, frame
round-trips, a cross-language check that drives the real `src/glr.py` encoder
through the Rust decoder, and an end-to-end run of the real gevent
bootstrap); and the binary `window` frame layout (`server.rs`), asserted
byte-for-byte so it stays in lockstep with the viewer's decoder.

**Viewer** — the TypeScript helpers are unit-tested with Bun:

```sh
bun install --cwd web --frozen-lockfile   # one-time: install viewer deps
bun run --cwd web test
```

`tests/wire.test.ts` is the counterpart to the Rust `server.rs` test: it builds a
binary `window` frame in the same layout and asserts `decodeWindow` round-trips
the header and typed-array columns (and rejects a truncated frame), plus the
`formatBytes` / `formatRate` header helpers. `tests/timeline.test.ts` covers
`formatTime`'s unit selection and boundaries. (The pure, side-effect-free helpers
live in `web/src/wire.ts` so they're importable without booting the app.)
