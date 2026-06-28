# greenlane

A live timeline profiler for **gevent** applications. greenlane attaches to a
running Python process, traces greenlet switches (and GC pauses), and streams
them to a fast, zoomable web timeline — so you can see *which greenlet ran when*,
what it was doing, and where the hub stalls.

It's a single self-contained binary: a Rust collector + HTTP/WebSocket server
with the web viewer embedded inside it.

## How it works

```
 ┌── target Python process ──┐         ┌──────── greenlane ────────┐
 │ bootstrap.py              │  UDS     │ collector (thread)        │   HTTP/WS
 │  • greenlet.settrace ─────┼────────▶ │  → columnar slice store ──┼─────────▶  browser
 │  • gc.callbacks           │  events  │ axum server (embedded UI) │   (WebGL timeline)
 └───────────────────────────┘         └───────────────────────────┘
```

1. greenlane binds a Unix socket and injects `bootstrap.py` into the target via
   **`sys.remote_exec`** (PEP 768, CPython **3.14+**).
2. The bootstrap registers a `greenlet.settrace` hook (and `gc.callbacks`) and
   streams switch/GC events back over the socket.
3. The Rust collector turns switches into per-greenlet run-interval *slices* and
   serves a live timeline; the embedded TypeScript/WebGL viewer renders it.

## Build

Requires **Rust** (edition 2024), **bun**, and **CPython 3.14+** on the target.

```sh
make build          # bun build the viewer + cargo build --release
# binary: target/release/greenlane
```

`make` targets: `web`, `build`, `run`, `linux` (static musl via `cross`),
`deploy` (cross-build + `kubectl cp` into a pod), `clean`.

## Usage

```sh
greenlane attach <PID> --serve 127.0.0.1:8080
# open http://127.0.0.1:8080
```

- `--serve <addr>` — run the web viewer (omit for a plain CLI summary).
- `--no-inject` — don't call `sys.remote_exec`; greenlane prints a bootstrap
  path for you to load yourself (`exec(open(path).read())`). Use on hosts where
  remote attach is blocked.
- `--python <bin>` — interpreter used to drive `sys.remote_exec` (default
  `python3`); must be 3.14+.

### Attaching — privileges

`sys.remote_exec` needs OS permission to access the target:

- **Linux:** run as root (`sudo`) or same-uid with a permissive
  `ptrace_scope` / `CAP_SYS_PTRACE`.
- **macOS (SIP on):** needs **both** root **and** the target interpreter signed
  with `get-task-allow`. Adhoc-re-sign once:
  ```sh
  codesign -s - -f --entitlements dbg.entitlements <python3.14 binary>
  # dbg.entitlements: com.apple.security.get-task-allow = true
  ```
  then restart the target and run under `sudo`. Or just use `--no-inject`.

## Viewer features

- **Timeline** — one lane per greenlet; span width = real run time. WebGL
  instanced rendering scales to millions of spans (cull + GPU instancing).
- **CPU graph** — busy fraction of the single gevent thread (non-Hub run time),
  time-aligned with the spans.
- **GC pauses** — global vertical lines marking each collection (hover for
  generation / duration / objects freed).
- **Highlights** — spans > 20 ms (yellow) / > 50 ms (red); Hub never flagged.
- **Slow log** — collapsible list of slow spans; filter by level, sort by
  time/duration, click to jump to it.
- **Trace panel** — click a span for its full call stack (file:line); click a
  frame to open it in your editor (VS Code / Cursor / Zed / PyCharm).
- **Sort lanes** by ident or by activity (1s / 10s / 60s / total).
- **Time axis** in relative / local-clock / UTC.
- Live **follow**, drag-to-**zoom-select**, pan, and per-greenlet detail.
- **Detach** — stop instrumenting; the bootstrap removes its hook from the
  target.

## Limitations

- The hook runs on the target's hot path; very high switch rates add overhead.
  Full-path call stacks are the largest per-event field (interned client-side).
- Per-span time precision is f32 ms relative to trace start (~µs over minutes).
- No server-side LOD yet — the browser holds the full timeline (fine for typical
  sessions; huge multi-hour captures would want viewport-scoped aggregation).
- Serves over plain HTTP with no auth — bind to `127.0.0.1` (use an SSH tunnel
  for remote viewing).
