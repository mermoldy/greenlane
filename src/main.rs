//! greenlane — attach to a running gevent process and profile greenlet switches.
//!
//! Approach:
//!   1. Bind a Unix STREAM socket.
//!   2. Inject `bootstrap.py` into the target via `sys.remote_exec` (PEP 768,
//!      CPython 3.14+). The bootstrap registers a `greenlet.settrace` hook and
//!      streams switch events back to us over that socket.
//!   3. Either record the events into a timeline store and serialize it to a
//!      `.glr` file (default), or (`--serve`) serve a live web viewer over HTTP.
//!      A recorded file is reopened later with `greenlane open <file>`.

mod db;
mod record;
mod server;
mod store;

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use db::Db;
use store::{GcEvent, Slice};

/// bootstrap_gevent.py with a `__SOCKET_PATH__` placeholder we substitute at runtime.
const BOOTSTRAP_TEMPLATE: &str = include_str!("bootstrap_gevent.py");

#[derive(Parser)]
#[command(
    name = "greenlane",
    about = "Attach to a running gevent process and profile greenlet switches"
)]
struct Cli {
    /// Log output format. `text` is human-readable; `json` emits one JSON
    /// object per line for ingestion by a log pipeline.
    #[arg(long, value_enum, default_value_t = LogFormat::Text, global = true)]
    log_format: LogFormat,
    #[command(subcommand)]
    cmd: Cmd,
}

/// How diagnostics are rendered. The level filter is independent — set it with
/// the standard `RUST_LOG` env var (defaults to `info`).
#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

/// Parse a viewer listen address, accepting friendly shorthands: a bare port
/// (`8080`) or `:8080` both bind `127.0.0.1`; a full `host:port` is taken as-is
/// (use `0.0.0.0:8080` to expose on the network).
fn parse_listen_addr(s: &str) -> Result<SocketAddr, String> {
    let candidate = if s.parse::<u16>().is_ok() {
        format!("127.0.0.1:{s}")
    } else if let Some(port) = s.strip_prefix(':') {
        format!("127.0.0.1:{port}")
    } else {
        s.to_string()
    };
    candidate
        .parse()
        .map_err(|e| format!("invalid listen address {s:?}: {e}"))
}

/// Install the global tracing subscriber. Honors `RUST_LOG` (falling back to
/// `info`) and writes to stderr so stdout stays free for any future piped output.
fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.with_target(false).init(),
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Attach to a running Python/gevent process by PID and record its timeline.
    ///
    /// By default the timeline is written to a `.glr` file (open it later with
    /// `greenlane open`). Pass `--serve` to watch it live in the browser; pass
    /// both to watch live *and* save the session on exit.
    Attach {
        /// Target process PID.
        pid: i32,
        /// Python interpreter used to trigger `sys.remote_exec` (must be 3.14+).
        #[arg(long, default_value = "python3")]
        python: String,
        /// Skip sys.remote_exec; just listen and print the bootstrap to load
        /// manually. Use on hosts where remote attach is blocked (e.g. macOS
        /// task-port restrictions) and you self-instrument your app instead.
        #[arg(long)]
        no_inject: bool,
        /// Serve the live web viewer. Pass bare (`--serve` → `127.0.0.1:8080`),
        /// a port (`--serve 9000` / `--serve :9000`), or a full address
        /// (`--serve 0.0.0.0:8080` to expose on the network). Omit entirely to
        /// record to a file instead.
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "127.0.0.1:8080",
            value_parser = parse_listen_addr,
        )]
        serve: Option<SocketAddr>,
        /// Write the recording to this path. Defaults to `greenlane-<pid>.glr`
        /// when not serving; ignored-unless-set when `--serve` is given (in
        /// which case it also saves the live session to the file on exit).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Serve viewer assets from this directory instead of the ones embedded
        /// in the binary (for frontend iteration).
        #[arg(long)]
        web_dir: Option<PathBuf>,
    },

    /// Open a recorded `.glr` timeline in the web viewer (static, not live).
    Open {
        /// Path to a `.glr` file written by `greenlane attach`.
        file: PathBuf,
        /// Address to serve the viewer at. Accepts a port, `:port`, or a full
        /// `host:port` (see `attach --serve`).
        #[arg(long, default_value = "127.0.0.1:8080", value_parser = parse_listen_addr)]
        serve: SocketAddr,
        /// Serve viewer assets from this directory instead of the embedded ones.
        #[arg(long)]
        web_dir: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_format);
    match cli.cmd {
        Cmd::Attach {
            pid,
            python,
            no_inject,
            serve,
            out,
            web_dir,
        } => attach(pid, &python, no_inject, serve, out, web_dir),
        Cmd::Open {
            file,
            serve,
            web_dir,
        } => open(&file, serve, web_dir),
    }
}

fn attach(
    pid: i32,
    python: &str,
    no_inject: bool,
    serve: Option<SocketAddr>,
    out: Option<PathBuf>,
    web_dir: Option<PathBuf>,
) -> Result<()> {
    // Fail fast with a clear message if there's nothing to attach to, rather
    // than letting the failure surface later as an opaque remote_exec error.
    ensure_pid_exists(pid)?;

    // Use a shared, world-accessible dir (NOT $TMPDIR): under `sudo` greenlane
    // runs as root while the target runs as the invoking user, so both the
    // socket and bootstrap must be reachable across that uid boundary.
    let base = PathBuf::from("/tmp");
    let sock_path = base.join(format!("greenlane-{pid}.sock"));
    let _ = std::fs::remove_file(&sock_path);

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding control socket at {}", sock_path.display()))?;
    // Let the (possibly non-root) target connect back to a root-owned socket.
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o777))?;
    listener.set_nonblocking(true)?;
    info!(socket = %sock_path.display(), "listening for target connection");

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .context("installing SIGINT handler")?;
    }

    let bootstrap_path = write_bootstrap(pid, &base, &sock_path)?;
    if no_inject {
        info!(
            "--no-inject — load this into the target yourself, e.g.\n    \
             exec(open({:?}).read())\nwaiting for connection…",
            bootstrap_path.to_string_lossy()
        );
    } else {
        inject(python, pid, &bootstrap_path)?;
        info!(pid, "injected bootstrap; waiting for connection…");
    }

    // Wait for the target to connect back (it runs remote_exec at its next safe point).
    let stream = match accept(&listener, &running)? {
        Some(s) => s,
        None => {
            cleanup(&sock_path, &bootstrap_path);
            return Ok(());
        }
    };

    // Where (if anywhere) to save: an explicit --out always wins; otherwise a
    // record-only attach (no --serve) defaults to `greenlane-<pid>.glr`.
    let out_path = match (&serve, out) {
        (_, Some(p)) => Some(p),
        (None, None) => Some(PathBuf::from(format!("greenlane-{pid}.glr"))),
        (Some(_), None) => None,
    };

    // Both modes feed the same DB-backed timeline; --serve also serves it live.
    let db = Db::spawn()?;
    let result = match serve {
        Some(addr) => {
            info!("connected — streaming to viewer (Ctrl-C to stop)");
            serve_mode(stream, running.clone(), pid, addr, web_dir, db.clone())
        }
        None => {
            let path = out_path
                .as_ref()
                .expect("record mode always has an out path");
            info!(path = %path.display(), "connected — recording (Ctrl-C to stop)");
            record_to_file(stream, &running, &db, pid, path)
        }
    };
    cleanup(&sock_path, &bootstrap_path);
    result?;

    if let Some(path) = out_path {
        db.flush_to_file(&path, pid)?;
        info!("wrote recording — open it with: greenlane open {}", path.display());
    }
    Ok(())
}

/// Materialize bootstrap.py with the real socket path baked in.
fn write_bootstrap(pid: i32, base: &PathBuf, sock_path: &PathBuf) -> Result<PathBuf> {
    let script = BOOTSTRAP_TEMPLATE.replace("__SOCKET_PATH__", &sock_path.to_string_lossy());
    let path = base.join(format!("greenlane-bootstrap-{pid}.py"));
    let mut f = std::fs::File::create(&path)
        .with_context(|| format!("writing bootstrap to {}", path.display()))?;
    f.write_all(script.as_bytes())?;
    // Readable by the target even when greenlane wrote it as root under sudo.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    Ok(path)
}

/// Verify the target PID is a live process before we set anything up. Uses
/// `kill(pid, 0)`, which probes for existence without delivering a signal:
/// `ESRCH` means no such process, `EPERM` means it exists but is owned by
/// another user (still attachable under sudo / as that user).
fn ensure_pid_exists(pid: i32) -> Result<()> {
    if pid <= 0 {
        bail!("Invalid PID {pid}: expected a positive process id.");
    }
    // Safe: kill with signal 0 performs no action other than error checking.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => bail!(
            "No process with PID {pid} is running.\n\
             Check the PID with `ps -p {pid}` or `pgrep -fl python`, then retry\n\
             with the correct process id."
        ),
        // Process exists but we can't signal it — that's a privilege issue, not
        // a missing target. Let attach proceed; inject() will give the precise
        // platform-specific permission guidance if it really is blocked.
        Some(libc::EPERM) => Ok(()),
        _ => Err(err).with_context(|| format!("checking whether PID {pid} exists")),
    }
}

/// Trigger `sys.remote_exec` from a helper interpreter. Shelling out is the
/// simplest reliable way to drive PEP 768; a native implementation can come later.
fn inject(python: &str, pid: i32, bootstrap_path: &PathBuf) -> Result<()> {
    let code = format!(
        "import sys; sys.remote_exec({pid}, {:?})",
        bootstrap_path.to_string_lossy()
    );
    let out = Command::new(python)
        .args(["-c", &code])
        .output()
        .with_context(|| format!("running {python} to call sys.remote_exec"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{}",
            diagnose_inject_failure(pid, out.status.code(), &stderr)
        );
    }
    Ok(())
}

/// Turn a raw `sys.remote_exec` failure into an actionable, platform-specific
/// message. We classify by signatures in the target interpreter's stderr so the
/// hint points at the one fix that actually applies, rather than a generic list.
fn diagnose_inject_failure(pid: i32, exit: Option<i32>, stderr: &str) -> String {
    let lower = stderr.to_lowercase();

    // 1. Privilege / OS-level process-access denial. On macOS this surfaces as a
    //    failure to obtain the Mach task port; on Linux as ptrace/EPERM.
    let is_perm = lower.contains("permissionerror")
        || lower.contains("task port")
        || lower.contains("kern_return_t")
        || lower.contains("operation not permitted")
        || lower.contains("ptrace")
        || lower.contains("eperm");

    // 2. Remote debugging compiled/configured off in the target.
    let is_disabled = lower.contains("remote") && lower.contains("disabled")
        || lower.contains("disable_remote_debug")
        || lower.contains("remote debugging is not enabled")
        || lower.contains("remote debugging is disabled");

    // 3. Target interpreter too old for PEP 768 (sys.remote_exec is 3.14+).
    let is_too_old = lower.contains("no attribute 'remote_exec'")
        || lower.contains("has no attribute \"remote_exec\"")
        || lower.contains("module 'sys' has no attribute");

    let mut msg = format!("Failed to inject into PID {pid} (sys.remote_exec exited {exit:?}).\n");

    let hint = if is_too_old {
        "\nCause: the target (or the helper interpreter) is older than Python 3.14.\n\
         sys.remote_exec / PEP 768 remote debugging requires CPython 3.14+ on both\n\
         the target process and the `--python` helper greenlane shells out to.\n\
         Fix: run the target under Python 3.14+, or point greenlane at a 3.14+\n\
         interpreter with `--python /path/to/python3.14`."
            .to_string()
    } else if is_disabled {
        "\nCause: remote debugging is turned off in the target interpreter.\n\
         Fix: ensure the target was NOT started with `-X disable_remote_debug` and\n\
         that `PYTHON_DISABLE_REMOTE_DEBUG` is unset in its environment. Also confirm\n\
         CPython wasn't built with `--without-remote-debug`."
            .to_string()
    } else if is_perm {
        format!(
            "\nCause: insufficient privileges to attach to PID {pid}.\n\
             greenlane must be able to access the target process, and the OS\n\
             guards that access.\n\
             {}",
            platform_permission_hint()
        )
    } else {
        format!(
            "\nCommon causes: target/helper Python < 3.14; insufficient privileges;\n\
             remote debugging disabled (PYTHON_DISABLE_REMOTE_DEBUG /\n\
             -X disable_remote_debug).\n\
             {}",
            platform_permission_hint()
        )
    };

    msg.push_str(&hint);
    if !stderr.trim().is_empty() {
        msg.push_str("\n\nTarget stderr:\n");
        for line in stderr.trim_end().lines() {
            msg.push_str("  ");
            msg.push_str(line);
            msg.push('\n');
        }
    }
    msg
}

/// Per-OS guidance for granting greenlane access to another process.
fn platform_permission_hint() -> String {
    if cfg!(target_os = "macos") {
        MACOS_PERMISSION_HINT.to_string()
    } else if cfg!(target_os = "linux") {
        LINUX_PERMISSION_HINT.to_string()
    } else {
        "Run greenlane with elevated privileges (e.g. sudo), or as the same user \
         that owns the target process."
            .to_string()
    }
}

const MACOS_PERMISSION_HINT: &str = r#"macOS: obtaining a target's Mach task port requires elevated rights.
The simplest fix is to run greenlane with sudo:

    sudo greenlane attach <PID> ...

To avoid sudo on every run, the greenlane binary can carry the
`com.apple.system-task-ports` entitlement and be code-signed. For local
development you can self-sign it:

    cat > gl.entitlements <<'EOF'
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
      "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0"><dict>
      <key>com.apple.system-task-ports</key><true/>
    </dict></plist>
    EOF
    codesign -s - -f --entitlements gl.entitlements ./target/debug/greenlane

Note: `com.apple.system-task-ports` is an Apple-private entitlement, so on a
stock (SIP-enabled) machine a self-signed binary may still be denied; running
under sudo is the reliable path. The target must also be owned by the same
user as greenlane (or run greenlane as that user / root)."#;

const LINUX_PERMISSION_HINT: &str = r#"Linux: attaching requires permission to ptrace the target. Options:

  1. Run greenlane as root, or as the same user that owns the target:

         sudo greenlane attach <PID> ...

  2. Grant the ptrace capability to the binary (no sudo per run):

         sudo setcap cap_sys_ptrace+ep $(command -v greenlane)

  3. Relax the Yama ptrace_scope restriction (system-wide; prefer 1 or 2):

         sudo sysctl kernel.yama.ptrace_scope=0

If the target runs inside a container, its process must also be visible from
greenlane's PID namespace."#;

/// Poll the nonblocking listener until the target connects or the user aborts.
fn accept(listener: &UnixListener, running: &AtomicBool) -> Result<Option<UnixStream>> {
    while running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => return Ok(Some(stream)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e).context("accepting target connection"),
        }
    }
    Ok(None)
}

/// One greenlet switch parsed from the event stream. A `throw` (greenlet killed
/// via `throw()`) is also a context switch, so we attribute it the same way.
struct SwitchEvent {
    ts: u64,
    target: u64,
    label: String,
    func: String,
    task: String,
    stack: String,
}

/// Parse one tab-delimited event line:
/// `t_ns \t event \t origin \t target \t label \t func \t task \t stack`.
fn parse_event(line: &str) -> Option<SwitchEvent> {
    let mut parts = line.split('\t');
    let ts = parts.next()?.parse::<u64>().ok()?;
    let _event = parts.next()?;
    let _origin = parts.next()?;
    let target = parts.next()?.parse::<u64>().ok()?;
    let label = parts.next().unwrap_or("").to_string();
    let func = parts.next().unwrap_or("").to_string();
    let task = parts.next().unwrap_or("").to_string();
    let stack = parts.next().unwrap_or("").to_string();
    Some(SwitchEvent {
        ts,
        target,
        label,
        func,
        task,
        stack,
    })
}

/// Parse the body of a GC line (after `gc\t`): `start \t dur \t gen \t collected`.
fn parse_gc(rest: &str) -> Option<GcEvent> {
    let mut p = rest.split('\t');
    let start = p.next()?.parse::<u64>().ok()?;
    let dur = p.next()?.parse::<u64>().ok()?;
    let generation = p.next()?.parse::<i64>().ok()?;
    let collected = p.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
    Some(GcEvent {
        start,
        dur,
        generation,
        collected,
    })
}

// ── Web viewer mode ────────────────────────────────────────────────────────

fn serve_mode(
    stream: UnixStream,
    running: Arc<AtomicBool>,
    pid: i32,
    addr: SocketAddr,
    web_dir: Option<PathBuf>,
    db: Db,
) -> Result<()> {
    // Flipped by POST /detach: stops the collector so the target self-removes
    // its trace hook (the broken socket triggers cleanup in the bootstrap).
    let detached = Arc::new(AtomicBool::new(false));

    // The blocking socket reader stays on its own std thread, feeding the DB.
    let collector = {
        let running = running.clone();
        let detached = detached.clone();
        let db = db.clone();
        std::thread::spawn(move || {
            if let Err(e) = read_slices(stream, &running, &detached, &db) {
                error!(error = format!("{e:#}"), "collector error");
            }
        })
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let shutdown = {
            let running = running.clone();
            async move {
                while running.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        };
        // Live attach: no recording source.
        server::serve(db, pid, detached.clone(), None, addr, web_dir, shutdown).await
    })?;

    running.store(false, Ordering::SeqCst);
    let _ = collector.join();
    Ok(())
}

/// How often a recording session reports progress and flushes a partial file.
const RECORD_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Drive a record-only session: drain the event stream on a worker thread while
/// this thread periodically reports progress and flushes a partial recording to
/// disk, so a hard kill (not just Ctrl-C) still leaves a usable `.glr`.
fn record_to_file(
    stream: UnixStream,
    running: &Arc<AtomicBool>,
    db: &Db,
    pid: i32,
    path: &Path,
) -> Result<()> {
    // No detach concept off-server; an empty flag keeps read_slices' signature.
    let no_detach = Arc::new(AtomicBool::new(false));
    let collector = {
        let running = running.clone();
        let db = db.clone();
        std::thread::spawn(move || read_slices(stream, &running, &no_detach, &db))
    };

    let mut next = Instant::now() + RECORD_REPORT_INTERVAL;
    while running.load(Ordering::SeqCst) && !collector.is_finished() {
        std::thread::sleep(Duration::from_millis(200));
        if Instant::now() >= next {
            flush_and_report(db, pid, path);
            next += RECORD_REPORT_INTERVAL;
        }
    }
    // Stop the collector if it's still running (Ctrl-C), then surface its result.
    running.store(false, Ordering::SeqCst);
    collector
        .join()
        .map_err(|_| anyhow::anyhow!("recording collector thread panicked"))?
}

/// Flush the current timeline to `path` and log event count + on-disk size.
fn flush_and_report(db: &Db, pid: i32, path: &Path) {
    match db.flush_to_file(path, pid) {
        Ok(bytes) => info!(events = db.total(), bytes, "recording…"),
        Err(e) => warn!(error = %format!("{e:#}"), "failed to flush partial recording"),
    }
}

/// Read the switch stream, closing one [`Slice`] per run-interval and ingesting
/// it into the DB. A greenlet's slice closes when the *next* switch arrives: the
/// time from being switched-in until away is attributed to whoever was running.
/// Slices/GC are batched before ingest to keep channel traffic down; the batch is
/// flushed on idle so a live viewer still sees fresh data promptly.
fn read_slices(
    stream: UnixStream,
    running: &AtomicBool,
    detached: &AtomicBool,
    db: &Db,
) -> Result<()> {
    /// Slices/GC buffered before a batched ingest.
    const BATCH: usize = 2048;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // (gid, start_ns, name, func, task, stack) for the currently-running
    // greenlet — captured when it was switched in (its resume point).
    let mut cur: Option<(u64, u64, String, String, String, String)> = None;
    let mut pending: Vec<Slice> = Vec::new();
    let mut pending_gc: Vec<GcEvent> = Vec::new();

    while running.load(Ordering::SeqCst) && !detached.load(Ordering::SeqCst) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                info!("target closed the connection");
                break;
            }
            Ok(n) => {
                // Count the raw stream volume processed (reported live in the
                // viewer header), regardless of how the line parses.
                db.add_bytes(n);
                let trimmed = line.trim_end();
                // Header line: "meta\t<epoch_ms>" — wall-clock at trace t0.
                if let Some(rest) = trimmed.strip_prefix("meta\t") {
                    if let Ok(ms) = rest.parse::<u64>() {
                        db.set_epoch(ms);
                    }
                    continue;
                }
                // GC pause: "gc\t<start_ns>\t<dur_ns>\t<gen>\t<collected>".
                if let Some(ev) = trimmed.strip_prefix("gc\t").and_then(parse_gc) {
                    pending_gc.push(ev);
                    if pending_gc.len() >= BATCH {
                        db.ingest_gc(std::mem::take(&mut pending_gc));
                    }
                    continue;
                }
                if let Some(ev) = parse_event(trimmed) {
                    if let Some((gid, start, name, func, task, stack)) = cur.take() {
                        pending.push(Slice {
                            gid,
                            start,
                            dur: ev.ts.saturating_sub(start),
                            name,
                            func,
                            task,
                            stack,
                        });
                        if pending.len() >= BATCH {
                            db.ingest_slices(std::mem::take(&mut pending));
                        }
                    }
                    cur = Some((ev.target, ev.ts, ev.label, ev.func, ev.task, ev.stack));
                }
            }
            // Idle: flush buffered events so the live viewer sees them promptly.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !pending.is_empty() {
                    db.ingest_slices(std::mem::take(&mut pending));
                }
                if !pending_gc.is_empty() {
                    db.ingest_gc(std::mem::take(&mut pending_gc));
                }
            }
            Err(e) => return Err(e).context("reading event stream"),
        }
    }
    // Final flush of anything still buffered.
    if !pending.is_empty() {
        db.ingest_slices(pending);
    }
    if !pending_gc.is_empty() {
        db.ingest_gc(pending_gc);
    }
    Ok(())
}

// ── Open a recorded timeline ───────────────────────────────────────────────

/// Load a `.glr` recording and serve the viewer over it. No collector and no
/// target connection — the store is fully populated up front and the session
/// is reported as detached (static), so the viewer renders it without trying
/// to stream more.
fn open(file: &Path, addr: SocketAddr, web_dir: Option<PathBuf>) -> Result<()> {
    let bytes = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    let db = Db::spawn()?;
    // Stream the file into the DB chunk-by-chunk (bounded memory on open).
    let pid = record::ingest_file(file, &db)?;
    info!(slices = db.total(), bytes, file = %file.display(), "loaded recording");
    let source = Some(server::Source {
        file: file.display().to_string(),
        bytes,
    });

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .context("installing SIGINT handler")?;
    }
    // A recording is never live; mark it detached so the viewer shows it static.
    let detached = Arc::new(AtomicBool::new(true));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let shutdown = {
            let running = running.clone();
            async move {
                while running.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        };
        server::serve(db, pid, detached, source, addr, web_dir, shutdown).await
    })?;
    Ok(())
}

fn cleanup(sock_path: &PathBuf, bootstrap_path: &PathBuf) {
    let _ = std::fs::remove_file(sock_path);
    let _ = std::fs::remove_file(bootstrap_path);
}
