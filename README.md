# greenlane

![version](https://img.shields.io/badge/version-0.1.0-blue)
![rust](https://img.shields.io/badge/rust-2024-orange)
![python](https://img.shields.io/badge/python-3.14%2B-3776AB)
![viewer](https://img.shields.io/badge/viewer-WebGL2-8A2BE2)
![slop](https://img.shields.io/badge/slop-100%25-brightgreen)

greenlane is a live timeline profiler for gevent applications. It attaches to a running
Python process and records scheduler activity and renders it on a zoomable web timeline. 
It shows the gevent hub scheduler, the garbage collector, and your own call stacks in
one view: which greenlet ran when, what it was doing, and where the hub stalls.

It uses [greenlet.settrace](https://greenlet.readthedocs.io/en/latest/api.html#greenlet.settrace) to observe every cooperative switch on the gevent hub thread.

## Install

Grab the tarball for your platform from the
[latest release](https://github.com/mermoldy/greenlane/releases/latest), verify
its checksum, and drop the binary on your `PATH`:

```sh
# Pick the artifact for your platform:
#   macOS (Apple Silicon)   greenlane-darwin_arm64.tar.gz
#   Linux x86_64 (static)   greenlane-linux_amd64.tar.gz
#   Linux arm64  (static)   greenlane-linux_arm64.tar.gz
ARTIFACT=greenlane-darwin_arm64.tar.gz
BASE=https://github.com/mermoldy/greenlane/releases/latest/download

curl -fsSL -O "$BASE/$ARTIFACT"
curl -fsSL -O "$BASE/$ARTIFACT.sha256"
shasum -a 256 -c "$ARTIFACT.sha256"          # verify; on Linux use: sha256sum -c

tar -xzf "$ARTIFACT"
sudo install -m 755 "${ARTIFACT%.tar.gz}/greenlane" /usr/local/bin/greenlane
```

Then check it runs:

```sh
greenlane --help
```

The viewer is embedded in the binary, so that's the whole install. The
target process you attach to must be running **CPython 3.14+** (see
[Attaching & permissions](#attaching--permissions)).

### Build from source

To build it yourself, you need Rust (edition 2024) and bun:

```sh
git clone https://github.com/mermoldy/greenlane
cd greenlane
make build                       # bundles the viewer + builds the release binary
sudo install -m 755 target/release/greenlane /usr/local/bin/
```

## Quick start

Attach to a running process by PID and watch it live in your browser:

```sh
greenlane attach <PID> --serve        # serves at http://127.0.0.1:8080
```

greenlane prints a capability URL with a one-time session token, like
`http://127.0.0.1:8080/?token=…`. Open that exact URL — it authorizes the viewer
(opening the bare address returns a 403). You'll see the timeline filling in live.
Press `Ctrl-C` to stop; the detach button in the viewer removes the hook and
leaves the process exactly as it was.

Don't know the PID? Find it with:

```sh
pgrep -fl python
```

> [!TIP]
> `--serve` accepts a bare port (`--serve 9000`), a `:port`, or a full
> `host:port`. Use `--serve 0.0.0.0:8080` to expose it on the network. greenlane
> prints a capability URL with a per-session token (`http://…/?token=…`) and gates
> `/ws`, `/info`, and `/detach` on it — open the printed URL, and only holders of
> the token can read the timeline or detach. It's plain HTTP, though, so for a
> remote host still prefer binding to `127.0.0.1` and reaching it over an SSH
> tunnel.

## Record and replay

Omit `--serve` and greenlane records the session to a `.glr` file instead of
serving it. Open that file any time to explore the exact same timeline in the
viewer — frozen instead of live.

```sh
greenlane attach <PID>                # records to greenlane-<PID>.glr
greenlane open greenlane-<PID>.glr    # replays it at http://127.0.0.1:8080
```

To do both, pass `--serve` to watch live _and_ `--out <path>` to also save the
session to disk on exit:

```sh
greenlane attach <PID> --serve --out session.glr
```

## Attaching & permissions

Attaching uses `sys.remote_exec` (PEP 768), so the OS needs to let greenlane
reach into the target. The usual fix is to run with elevated privileges:

```sh
# Linux — run as root (or the target's owner)
sudo greenlane attach <PID> --serve

# macOS — obtaining the target's task port requires root
sudo greenlane attach <PID> --serve
```

If `attach` fails, greenlane prints the specific cause and its fix (wrong PID,
Python older than 3.14, remote debugging disabled, or insufficient privileges).
To attach without `sudo` every time — Linux `setcap`, the macOS
`com.apple.system-task-ports` entitlement, container PID namespaces, and the rest
— see the [full troubleshooting guide](docs/architecture.md#attaching--full-requirements--troubleshooting).

When injection is blocked entirely, `--no-inject` skips it and prints a bootstrap
path for you to load into the target yourself.

## Finding slow executions

Spans that run long enough to stall the scheduler are tinted on the timeline
(yellow past ≈20 ms, red past ≈50 ms — tune with `--warn-ms` / `--block-ms`) and
collected into a **slow log** docked at the bottom of the viewer. It's a query
over the _whole_ capture (not just what's on screen), so its badge is the true
count; filter it by tier, sort by time or duration, and click a row to jump the
timeline straight to that execution.

Click any execution to open its detail panel.:

```sh
greenlane attach <PID> --serve                 # slow (default): stacks for slow executions
greenlane attach <PID> --include-traces all --serve   # every execution
```

`--include-traces` takes `off`, `slow`, or `all`, defaulting to **`slow`** (a bare
`--include-traces` also means `slow`):

- **`slow`** (default) — capture the full stack **only for executions at/over the warn
  threshold** (`--warn-ms`, default 20 ms). Walking the Python stack is the
  hot-path cost, so it's done at a execution's _close_ (when its duration is known) and
  only for the slow executions you'd actually investigate. Cheap enough to leave on.
- **`all`** — full stack for every execution. Exhaustive, but walks on every greenlet
  switch — real overhead on high-switch-rate apps.
- **`off`** — no full stacks at all.

Every execution always carries its cheap leaf-function label regardless of mode. The
captured stack is the greenlet/task's **yield point** — where it was when it gave
up control (often the blocking call), which is usually what you want for "why was
this execution slow". Use `all`/`slow` to investigate _where_ time goes; `off` for the
lowest-overhead steady-state monitoring.

## Learn more

How greenlane works under the hood — the injection handshake, the event
pipeline, the lossless streaming model, the full viewer tour, and known
limitations — is documented in **[docs/architecture.md](docs/architecture.md)**.

If you need a target to attach to, the demo load generator (`bench/app.py`) and
how to run the automated checks are covered in **[docs/testing.md](docs/testing.md)**.

Useful flags:

- `--include-traces <off|slow|all>` — full call-stack capture (default `slow`:
  stacks for executions over `--warn-ms`; `all` for every execution; `off` for none — see
  [Finding slow executions](#finding-slow-executions)).
- `--warn-ms <n>` / `--block-ms <n>` — slow-execution highlight + slow-log thresholds
  (default 20 / 50 ms).
- `--python <bin>` — helper interpreter that drives `sys.remote_exec` (3.14+).
- `--out <path>` — where to save the recording.
- `--log-format <text|json>` and `RUST_LOG` — diagnostics (all go to stderr).
