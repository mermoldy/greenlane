# greenlane

![version](https://img.shields.io/badge/version-0.1.0-blue)
![rust](https://img.shields.io/badge/rust-2024-orange)
![python](https://img.shields.io/badge/python-3.14%2B-3776AB)
![viewer](https://img.shields.io/badge/viewer-WebGL2-8A2BE2)
![slop](https://img.shields.io/badge/slop-100%25-brightgreen)

A live timeline profiler for **gevent** and **asyncio** applications. greenlane
attaches to a running Python process and streams a fast, zoomable web timeline of
_which unit of work ran when_, what it was doing, and where the scheduler stalls
— no code changes, no restart.

It is a single self-contained binary. The web viewer is baked in, and the only
thing that ever touches your process is a small bootstrap greenlane injects at
attach time and removes again when you detach.

## Install

Grab a prebuilt binary from the
[latest release](https://github.com/mermoldy/greenlane/releases/latest) and drop
it on your `PATH`:

```sh
# macOS (Apple Silicon)
curl -fsSL https://github.com/mermoldy/greenlane/releases/latest/download/greenlane-aarch64-apple-darwin -o greenlane

# Linux (x86_64, static)
curl -fsSL https://github.com/mermoldy/greenlane/releases/latest/download/greenlane-x86_64-unknown-linux-musl -o greenlane

chmod +x greenlane
sudo install -m 755 greenlane /usr/local/bin/
```

Then check it runs:

```sh
greenlane --help
```

The viewer is embedded in the binary, so that's the whole install. The
**target** process you attach to must be running **CPython 3.14+** (see
[Attaching & permissions](#attaching--permissions)).

> Homebrew support is coming.

### Build from source

Prefer to build it yourself? You need **Rust** (edition 2024) and **bun**:

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

Open <http://127.0.0.1:8080> and you'll see the timeline filling in live. Press
`Ctrl-C` to stop; the detach button in the viewer removes the hook and leaves the
process exactly as it was.

Don't know the PID? Find it with:

```sh
pgrep -fl python
```

> [!TIP]
> `--serve` accepts a bare port (`--serve 9000`), a `:port`, or a full
> `host:port`. Use `--serve 0.0.0.0:8080` to expose it on the network — but the
> viewer has no authentication, so prefer binding to `127.0.0.1` and reaching a
> remote host over an SSH tunnel.

## Record now, replay later

Omit `--serve` and greenlane records the session to a `.glr` file instead of
serving it. Open that file any time to explore the exact same timeline in the
viewer — frozen instead of live.

```sh
greenlane attach <PID>                # records to greenlane-<PID>.glr
greenlane open greenlane-<PID>.glr    # replays it at http://127.0.0.1:8080
```

Want both at once? Pass `--serve` to watch live _and_ `--out <path>` to also save
the session to disk on exit:

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

## Learn more

How greenlane works under the hood — the injection handshake, the event
pipeline, the lossless streaming model, the full viewer tour, and known
limitations — is documented in **[docs/architecture.md](docs/architecture.md)**.

Useful flags:

- `--python <bin>` — helper interpreter that drives `sys.remote_exec` (3.14+).
- `--out <path>` — where to save the recording.
- `--log-format <text|json>` and `RUST_LOG` — diagnostics (all go to stderr).
