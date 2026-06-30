//! greenlane — attach to a running gevent process and profile its scheduler
//! activity (greenlet switches).
//!
//! Approach:
//!   1. Bind a Unix STREAM socket.
//!   2. Inject the gevent bootstrap (`greenlet.settrace`) into the target via
//!      `sys.remote_exec` (PEP 768, CPython 3.14+). The bootstrap streams binary
//!      trace frames back to us over that socket.
//!   3. Either record the events into a timeline store and serialize it to a
//!      `.glr` file (default), or (`--serve`) serve a live web viewer over HTTP.
//!      A recorded file is reopened later with `greenlane open <file>`.

mod db;
mod record;
mod server;
mod store;
mod sysinfo;
mod trace_format;

use std::io::{Read, Write};
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
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use db::{Db, FuncAgg, GcGen, GreenletAgg, Query, Reply, SlowRow, SlowTier};
use store::{Execution, GcEvent};

/// The gevent bootstrap, with `__SOCKET_PATH__` / `__TRACE_MODE__` / `__LONG_NS__`
/// placeholders (and a `# __GLR_ENCODER__` marker) we substitute at runtime.
const BOOTSTRAP: &str = include_str!("bootstrap.py");

/// The shared Python binary-trace encoder (mirrors `src/trace_format.rs`). It is
/// inlined into the bootstrap at materialization, replacing the `# __GLR_ENCODER__`
/// marker, so the injected script stays a single self-contained file.
const GLR_ENCODER: &str = include_str!("glr.py");

#[derive(Parser)]
#[command(
    name = "greenlane",
    version,
    about = "Attach to a running gevent process and profile scheduler activity"
)]
struct Cli {
    /// Log output format. text is human-readable; json emits one JSON
    /// object per line for ingestion by a log pipeline.
    #[arg(long, value_enum, default_value_t = LogFormat::Text, global = true)]
    log_format: LogFormat,
    /// Verbose debug logging: per-batch streaming, DB reads/writes, the inject
    /// handshake, and other internals. Equivalent to RUST_LOG=info,greenlane=debug
    /// (an explicit RUST_LOG still wins if set).
    #[arg(long, global = true)]
    debug: bool,
    /// "Long" threshold (ms): executions at least this long are highlighted yellow and
    /// listed in the slow log. Also via GREENLANE_LONG_MS.
    #[arg(long, env = "GREENLANE_LONG_MS", default_value_t = 20, global = true)]
    long_ms: u64,
    /// "Blocked" threshold (ms): executions at least this long are highlighted red — long
    /// enough to stall the scheduler. Also via GREENLANE_BLOCKED_MS.
    #[arg(
        long,
        env = "GREENLANE_BLOCKED_MS",
        default_value_t = 50,
        global = true
    )]
    blocked_ms: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Long/blocked execution-duration thresholds (ns), configurable via flags/env. Drive
/// the slow-log filter + percentile context (server) and execution highlight colors
/// (client, via `meta`), and the long/blocked debug logging during capture.
#[derive(Clone, Copy)]
pub struct Thresholds {
    pub long_ns: u64,
    pub blocked_ns: u64,
}

/// How diagnostics are rendered. The level filter is independent — set it with
/// the standard `RUST_LOG` env var (defaults to `info`).
#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

/// How much call-stack detail the bootstrap captures. The stack walk is the
/// hot-path cost, so it's gated: it runs at each execution's *close* (when its duration
/// is known) on the greenlet/task that just yielded.
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq)]
enum TraceMode {
    /// No full stacks — only the cheap leaf-function label per execution.
    Off,
    /// Full stack only for executions at/over the long threshold (the default): the
    /// walk runs solely for the slow executions you'd actually investigate.
    Slow,
    /// Full stack for every execution (exhaustive; walks on every switch).
    All,
}

impl TraceMode {
    /// Wire encoding handed to the bootstrap (`0` off, `1` slow, `2` all).
    fn as_wire(self) -> u8 {
        match self {
            TraceMode::Off => 0,
            TraceMode::Slow => 1,
            TraceMode::All => 2,
        }
    }

    /// Stable lowercase name sent to the viewer (so the detail panel can explain
    /// per execution why a stack is or isn't present).
    fn as_str(self) -> &'static str {
        match self {
            TraceMode::Off => "off",
            TraceMode::Slow => "slow",
            TraceMode::All => "all",
        }
    }
}

/// Output format for `greenlane analyze`.
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq)]
enum AnalyzeFormat {
    /// Human-readable report (default).
    Text,
    /// One JSON object, for piping into jq / CI.
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

/// Install the global tracing subscriber. Writes to stderr so stdout stays free
/// for any future piped output. Level precedence: an explicit `RUST_LOG` always
/// wins; otherwise `--debug` raises this crate to `debug` (deps stay at `info`),
/// and the bare default is `info`.
fn init_logging(format: LogFormat, debug: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(if debug {
            "info,greenlane=debug"
        } else {
            "info"
        })
    });
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
    /// Attach to a running gevent process by PID and record its timeline.
    ///
    /// By default the timeline is written to a .glr file (open it later with
    /// greenlane open). Pass --serve to watch it live in the browser; pass
    /// both to watch live *and* save the session on exit.
    Attach {
        /// Target process PID.
        pid: i32,
        /// Python interpreter used to trigger sys.remote_exec (must be 3.14+).
        #[arg(long, default_value = "python3")]
        python: String,
        /// Skip sys.remote_exec; just listen and print the bootstrap to load
        /// manually. Use on hosts where remote attach is blocked (e.g. macOS
        /// task-port restrictions) and you self-instrument your app instead.
        #[arg(long)]
        no_inject: bool,
        /// Full call-stack capture mode: off, slow (default), or all.
        /// Walking the Python stack is the hot-path cost, so it's gated to a execution's
        /// close (when its duration is known): slow walks only executions at/over the
        /// long threshold (--long-ms) — the ones worth investigating — all
        /// walks every execution, off keeps only the cheap leaf label. Bare
        /// --include-traces means slow. Every execution always carries its cheap
        /// leaf-function label regardless.
        #[arg(
            long,
            value_enum,
            num_args = 0..=1,
            default_value_t = TraceMode::Slow,
            default_missing_value = "slow",
        )]
        include_traces: TraceMode,
        /// Serve the live web viewer. Pass bare (--serve → 127.0.0.1:8080),
        /// a port (--serve 9000 / --serve :9000), or a full address
        /// (--serve 0.0.0.0:8080 to expose on the network). Omit entirely to
        /// record to a file instead.
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "127.0.0.1:8080",
            value_parser = parse_listen_addr,
        )]
        serve: Option<SocketAddr>,
        /// Write the recording to this path. Defaults to greenlane-<pid>.glr
        /// when not serving; ignored-unless-set when --serve is given (in
        /// which case it also saves the live session to the file on exit).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Serve viewer assets from this directory instead of the ones embedded
        /// in the binary (for frontend iteration).
        #[arg(long)]
        web_dir: Option<PathBuf>,
        /// Disable the viewer's session-token auth. By default greenlane gates
        /// /ws and /detach on the token in the printed capability
        /// URL; with --no-auth the bare URL works and anyone who can reach the
        /// port has full access. Use only on a trusted, local-only bind.
        #[arg(long)]
        no_auth: bool,
    },

    /// Open a recorded .glr timeline in the web viewer (static, not live).
    Open {
        /// Path to a .glr file written by greenlane attach.
        file: PathBuf,
        /// Address to serve the viewer at. Accepts a port, :port, or a full
        /// host:port (see attach --serve).
        #[arg(long, default_value = "127.0.0.1:8080", value_parser = parse_listen_addr)]
        serve: SocketAddr,
        /// Serve viewer assets from this directory instead of the embedded ones.
        #[arg(long)]
        web_dir: Option<PathBuf>,
        /// Disable the viewer's session-token auth (see attach --no-auth).
        #[arg(long)]
        no_auth: bool,
    },

    /// Analyze a recorded .glr timeline and print a headless report without browser.
    ///
    /// Loads the recording into the same store the viewer uses and runs the same
    /// queries (top stalls, GC pressure, greenlet imbalance, hottest functions), then
    /// prints the result as text or JSON. Useful in CI or for quick triage over SSH.
    Analyze {
        /// Path to a .glr file written by greenlane attach.
        file: PathBuf,
        /// Output format: text (default, human-readable) or json.
        #[arg(long, value_enum, default_value_t = AnalyzeFormat::Text)]
        format: AnalyzeFormat,
        /// How many rows to show in the ranked lists (slowest executions, top greenlets,
        /// hottest functions).
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_format, cli.debug);
    let thresholds = Thresholds {
        long_ns: cli.long_ms * 1_000_000,
        blocked_ns: cli.blocked_ms * 1_000_000,
    };
    match cli.cmd {
        Cmd::Attach {
            pid,
            python,
            no_inject,
            include_traces,
            serve,
            out,
            web_dir,
            no_auth,
        } => attach(
            pid,
            &python,
            no_inject,
            include_traces,
            serve,
            out,
            web_dir,
            no_auth,
            thresholds,
        ),
        Cmd::Open {
            file,
            serve,
            web_dir,
            no_auth,
        } => open(&file, serve, web_dir, no_auth, thresholds),
        Cmd::Analyze { file, format, top } => analyze(&file, format, top, thresholds),
    }
}

#[allow(clippy::too_many_arguments)]
fn attach(
    pid: i32,
    python: &str,
    no_inject: bool,
    include_traces: TraceMode,
    serve: Option<SocketAddr>,
    out: Option<PathBuf>,
    web_dir: Option<PathBuf>,
    no_auth: bool,
    thresholds: Thresholds,
) -> Result<()> {
    // Fail fast with a clear message if there's nothing to attach to, rather
    // than letting the failure surface later as an opaque remote_exec error.
    ensure_pid_exists(pid)?;

    // A per-attach **private** directory under /tmp with an UNPREDICTABLE name
    // (16 random bytes) and mode 0711. Predictable, world-writable paths in /tmp
    // are raceable under sudo / on a multi-user host (an attacker pre-creates the
    // path or a symlink). The random name + 0711 (others may traverse to the
    // known-by-pathname socket/bootstrap, but not list or create) closes that,
    // while still letting the target — a different uid under sudo — reach the files
    // by their exact paths. We never `remove_file` first (the name can't pre-exist).
    let base =
        make_private_dir().context("creating private working directory for the attach session")?;
    let sock_path = base.join("control.sock");

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding control socket at {}", sock_path.display()))?;
    // Let the (possibly non-root) target connect back to a root-owned socket
    // (connect needs write permission on the socket inode).
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;
    info!(socket = %sock_path.display(), "listening for target connection");

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .context("installing SIGINT handler")?;
    }

    let bootstrap_path =
        write_bootstrap(pid, &base, &sock_path, include_traces, thresholds.long_ns)?;
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
            cleanup(&base);
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
    // Cap the in-memory timeline ONLY for ephemeral live-view-only sessions
    // (serve, not recording) so we don't OOM on an endless attach; recording or
    // serve-with-recording keeps everything so the `.glr` stays complete.
    let cap_rows = if serve.is_some() && out_path.is_none() {
        Some(LIVE_VIEW_MAX_ROWS)
    } else {
        None
    };
    let db = Db::spawn(cap_rows)?;
    let result = match serve {
        Some(addr) => {
            info!("connected — streaming to viewer (Ctrl-C to stop)");
            serve_mode(
                stream,
                running.clone(),
                pid,
                addr,
                web_dir,
                no_auth,
                out_path.clone(),
                db.clone(),
                thresholds,
                include_traces,
            )
        }
        None => {
            let path = out_path
                .as_ref()
                .expect("record mode always has an out path");
            info!(path = %path.display(), "connected — recording (Ctrl-C to stop)");
            record_to_file(stream, &running, &db, pid, path, thresholds)
        }
    };
    cleanup(&base);
    result?;

    if let Some(path) = out_path {
        db.flush_to_file(&path, pid)?;
        info!(
            "wrote recording — open it with: greenlane open {}",
            path.display()
        );
    }
    Ok(())
}

/// Substitute the socket path, trace mode, and long threshold into a bootstrap
/// template, and inline the shared binary-trace encoder where the marker appears.
fn fill_template(
    template: &str,
    sock_path: &Path,
    include_traces: TraceMode,
    long_ns: u64,
) -> String {
    template
        .replace("# __GLR_ENCODER__", GLR_ENCODER)
        .replace("__SOCKET_PATH__", &sock_path.to_string_lossy())
        .replace("__TRACE_MODE__", &include_traces.as_wire().to_string())
        .replace("__LONG_NS__", &long_ns.to_string())
}

/// 16 bytes from the OS CSPRNG (`/dev/urandom`) as lowercase hex (32 chars). The
/// single source for the unpredictable strings greenlane needs: the private-dir name
/// here and the viewer session token (see `server::random_token`).
pub(crate) fn random_hex16() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Create a private per-attach working directory `/tmp/greenlane-<random>` with an
/// unpredictable name and mode 0711, and return its path. The randomness (16 bytes
/// from `/dev/urandom`) defeats pre-creation/symlink races on the shared /tmp;
/// 0711 lets the target traverse to the known-by-path socket/bootstrap without
/// being able to list or create entries.
fn make_private_dir() -> Result<PathBuf> {
    let token = random_hex16().context("reading random bytes for the working-directory name")?;
    // Anchor under /tmp, NOT std::env::temp_dir(): on macOS the latter is a long
    // per-user path under /var/folders/…, and a Unix socket path has a hard
    // ~104-byte limit (SUN_LEN). `/tmp/greenlane-<32hex>/control.sock` is ~60
    // bytes and well within it on both macOS and Linux. /tmp is also reachable
    // across the uid boundary when greenlane runs under sudo and the target does
    // not. The unpredictable name + 0711 below preserve the anti-race hardening.
    let dir = PathBuf::from("/tmp").join(format!("greenlane-{token}"));
    // create_dir (not create_dir_all) fails if the path already exists, so a
    // pre-created collision can't be hijacked.
    std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711))?;
    Ok(dir)
}

/// Write a script to `base/<name>` readable by the target (even when greenlane
/// wrote it as root under sudo) and return its path. Uses `create_new` (O_EXCL):
/// it refuses to write through a pre-existing file or symlink.
fn write_script(base: &Path, name: &str, body: &str) -> Result<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = base.join(name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)
        .with_context(|| format!("writing {} to {}", name, path.display()))?;
    f.write_all(body.as_bytes())?;
    Ok(path)
}

/// Materialize the gevent bootstrap the target will exec, with the real socket
/// path + trace flag baked in, and return the path to inject.
fn write_bootstrap(
    pid: i32,
    base: &Path,
    sock_path: &Path,
    include_traces: TraceMode,
    long_ns: u64,
) -> Result<PathBuf> {
    write_script(
        base,
        &format!("greenlane-bootstrap-{pid}.py"),
        &fill_template(BOOTSTRAP, sock_path, include_traces, long_ns),
    )
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
             Check the PID with: ps -p {pid}   (or: pgrep -fl python), then retry\n\
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
fn inject(python: &str, pid: i32, bootstrap_path: &Path) -> Result<()> {
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
         the target process and the --python helper greenlane shells out to.\n\
         Fix: run the target under Python 3.14+, or point greenlane at a 3.14+\n\
         interpreter with --python /path/to/python3.14."
            .to_string()
    } else if is_disabled {
        "\nCause: remote debugging is turned off in the target interpreter.\n\
         Fix: ensure the target was NOT started with -X disable_remote_debug and\n\
         that PYTHON_DISABLE_REMOTE_DEBUG is unset in its environment. Also confirm\n\
         CPython wasn't built with --without-remote-debug."
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
com.apple.system-task-ports entitlement and be code-signed. For local
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

Note: com.apple.system-task-ports is an Apple-private entitlement, so on a
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

// ── Web viewer mode ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn serve_mode(
    stream: UnixStream,
    running: Arc<AtomicBool>,
    pid: i32,
    addr: SocketAddr,
    web_dir: Option<PathBuf>,
    no_auth: bool,
    out_path: Option<PathBuf>,
    db: Db,
    thresholds: Thresholds,
    include_traces: TraceMode,
) -> Result<()> {
    // Flipped by POST /detach: stops the collector so the target self-removes
    // its trace hook (the broken socket triggers cleanup in the bootstrap).
    let detached = Arc::new(AtomicBool::new(false));

    // Host/process/runtime introspection + scheduler-lag sampling (System panel via meta; lag/CPU samples into the store).
    let sys = sysinfo::SysInfo::new(pid);

    // The blocking socket reader stays on its own std thread, feeding the DB.
    let collector = {
        let running = running.clone();
        let detached = detached.clone();
        let db = db.clone();
        let sys_for_collector = (sys.clone(), running.clone());
        std::thread::spawn(move || {
            if let Err(e) = read_executions(
                stream,
                &running,
                &detached,
                &db,
                Some(sys_for_collector),
                thresholds,
            ) {
                error!(error = format!("{e:#}"), "collector error");
            }
        })
    };

    // Live + record (--serve --out): periodically seal a recording segment during
    // the session, exactly like record-only mode — so a hard kill keeps everything
    // up to the last seal (a final seal also runs on clean exit), and the recording
    // grows as bounded per-flush segments instead of one giant segment on exit.
    let recorder = out_path.map(|path| {
        let running = running.clone();
        let db = db.clone();
        std::thread::spawn(move || {
            let mut next = Instant::now() + RECORD_REPORT_INTERVAL;
            while running.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
                if Instant::now() >= next {
                    flush_and_report(&db, pid, &path);
                    next += RECORD_REPORT_INTERVAL;
                }
            }
        })
    });

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
        // Live attach: no recording source. `traces` tells the viewer whether to
        // expect full stacks at all (true unless mode is off); it detects per-execution
        // emptiness on its own.
        server::serve(
            db,
            pid,
            detached.clone(),
            None,
            Some(include_traces.as_str()),
            Some(sys),
            thresholds,
            addr,
            web_dir,
            no_auth,
            shutdown,
        )
        .await
    })?;

    running.store(false, Ordering::SeqCst);
    let _ = collector.join();
    if let Some(recorder) = recorder {
        let _ = recorder.join();
    }
    Ok(())
}

/// How often a recording session reports progress and flushes a partial file.
const RECORD_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// In-memory row cap for a live-view-only session (no recording): oldest executions
/// are evicted past this so an endless attach stays bounded. ~5M rows is roughly
/// a few hundred MB of columnar data; panning before the eviction horizon is empty.
const LIVE_VIEW_MAX_ROWS: usize = 5_000_000;

/// Drive a record-only session: drain the event stream on a worker thread while
/// this thread periodically reports progress and flushes a partial recording to
/// disk, so a hard kill (not just Ctrl-C) still leaves a usable `.glr`.
fn record_to_file(
    stream: UnixStream,
    running: &Arc<AtomicBool>,
    db: &Db,
    pid: i32,
    path: &Path,
    thresholds: Thresholds,
) -> Result<()> {
    // No detach concept off-server; an empty flag keeps read_executions' signature.
    let no_detach = Arc::new(AtomicBool::new(false));
    let collector = {
        let running = running.clone();
        let db = db.clone();
        std::thread::spawn(move || {
            read_executions(stream, &running, &no_detach, &db, None, thresholds)
        })
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

/// Whether a greenlet label denotes the Hub (waiting in the event loop, not real
/// work) — matches the server/client convention. Never flagged as a slow execution.
/// The single source of truth for the "Hub" prefix test (also used by `db::build_df`).
pub(crate) fn is_hub(name: &str) -> bool {
    name.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("hub"))
}

/// Read the binary frame stream (see [`trace_format`]), closing one [`Execution`] per
/// run-interval and ingesting it into the DB. A greenlet's execution closes when the
/// *next* switch on its thread arrives: the time from being switched-in until away
/// is attributed to whoever was running. Executions/GC are batched before ingest to
/// keep channel traffic down; the batch is flushed on idle so a live viewer still
/// sees fresh data promptly.
fn read_executions(
    stream: UnixStream,
    running: &AtomicBool,
    detached: &AtomicBool,
    db: &Db,
    sys: Option<(Arc<sysinfo::SysInfo>, Arc<AtomicBool>)>,
    thresholds: Thresholds,
) -> Result<()> {
    use trace_format::{Decoder, Item, Step};

    /// Upper bound on a single batch (caps a huge burst); the time-based flush
    /// below is what bounds *latency* when events only trickle in.
    const BATCH: usize = 2048;
    /// Flush partially-filled batches at least this often. Size-based batching
    /// alone holds a slow trickle until it reaches BATCH (minutes at low rates),
    /// so the live viewer would lag badly; this caps end-to-end lag to ~50ms.
    const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
    let mut stream = stream;
    // Short read timeout so the idle (no-data) path also flushes promptly.
    stream.set_read_timeout(Some(Duration::from_millis(
        FLUSH_INTERVAL.as_millis() as u64
    )))?;

    let mut dec = Decoder::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 65536];
    // Per-thread currently-running greenlet, keyed by the switch's OS thread id:
    // (gid, start_ns, name, func, task), captured when it was switched in (its
    // resume point). The full `stack` is NOT stored here — it arrives on the switch
    // that CLOSES this execution (the bootstrap walks the yielding greenlet's frame at
    // close, gated by trace mode), and is applied then. A switch closes only its
    // OWN thread's prior interval, so concurrent gevent hubs (multiple hub threads
    // streaming over the one socket) don't truncate each other.
    let mut cur: std::collections::HashMap<u64, (u64, u64, String, String, String)> =
        std::collections::HashMap::new();
    // Last event timestamp seen PER THREAD — used to close that thread's still-open
    // interval on the final flush. Keeping it per-thread (not one global max) means
    // a thread that goes quiet while another keeps running doesn't get its last execution
    // inflated to the other thread's activity.
    let mut last_ts: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut pending: Vec<Execution> = Vec::new();
    let mut pending_gc: Vec<GcEvent> = Vec::new();
    let mut last_flush = Instant::now();

    // Set if the framed stream goes corrupt: the format is stateful (interned pools,
    // ts base) and has no resync marker, so we can't reliably continue decoding — but
    // we MUST NOT kill the collector with an error (that froze ingestion silently).
    // Instead we stop cleanly, keep everything decoded so far, and let the stall
    // monitor surface it to the viewer.
    let mut decode_err = false;
    while running.load(Ordering::SeqCst) && !detached.load(Ordering::SeqCst) {
        // Drain every complete frame currently buffered.
        let mut consumed = 0usize;
        loop {
            let step = match dec.step(&buf[consumed..]) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        error = %format!("{e:#}"),
                        consumed,
                        buffered = buf.len(),
                        "event stream decode error — stopping collector (stream corrupt); \
                         keeping the executions captured so far"
                    );
                    decode_err = true;
                    break;
                }
            };
            match step {
                Step::NeedMore => break,
                Step::Done { item, consumed: n } => {
                    consumed += n;
                    match item {
                        Some(Item::Meta(m)) => {
                            if m.epoch_ms > 0 {
                                db.set_epoch(m.epoch_ms);
                            }
                            if let Some((sys, run)) = &sys {
                                if m.tid > 0 {
                                    sys.set_tid(m.tid, run.clone(), db.clone());
                                }
                                if !m.pyinfo.is_empty() {
                                    sys.set_pyinfo(&m.pyinfo);
                                }
                            }
                        }
                        Some(Item::Switch(ev)) => {
                            last_ts.insert(ev.thread, ev.ts);
                            if let Some((gid, start, name, func, task)) = cur.remove(&ev.thread) {
                                let dur = ev.ts.saturating_sub(start);
                                // Surface long/blocked non-Hub executions as they
                                // close, so `--debug` shows stalls live during capture.
                                if dur >= thresholds.long_ns && !is_hub(&name) {
                                    let level = if dur >= thresholds.blocked_ns {
                                        "blocked"
                                    } else {
                                        "long"
                                    };
                                    debug!(
                                        level,
                                        greenlet = %name,
                                        dur_ms = dur / 1_000_000,
                                        func = %func,
                                        "slow execution"
                                    );
                                }
                                // `stack` describes the execution being CLOSED (the
                                // greenlet that just yielded), captured at close by
                                // the bootstrap — empty unless the trace mode (and,
                                // for `slow`, the duration) called for a walk.
                                pending.push(Execution {
                                    gid,
                                    start,
                                    dur,
                                    name,
                                    func,
                                    task,
                                    stack: ev.stack,
                                });
                                if pending.len() >= BATCH {
                                    db.ingest_executions(std::mem::take(&mut pending));
                                }
                            }
                            cur.insert(ev.thread, (ev.target, ev.ts, ev.label, ev.func, ev.task));
                        }
                        Some(Item::Gc(g)) => {
                            pending_gc.push(g);
                            if pending_gc.len() >= BATCH {
                                db.ingest_gc(std::mem::take(&mut pending_gc));
                            }
                        }
                        // `execution` + `syssample` events are file-only / sampler-fed;
                        // the target never emits them on the wire.
                        Some(Item::Execution(_)) | Some(Item::Sample(_)) | None => {}
                    }
                    if consumed >= buf.len() {
                        break;
                    }
                }
            }
        }
        if consumed > 0 {
            buf.drain(0..consumed);
        }
        // Stream went corrupt: stop the read loop now (the final flush below keeps
        // everything decoded up to here). The session's data freezes at this point;
        // the server's stall monitor flags it so the viewer stops chasing the edge.
        if decode_err {
            break;
        }

        // Time-bounded flush: push what we've decoded so far if enough time has
        // passed, even while bytes keep arriving (so the idle branch below never
        // fires). Without this, a slow trickle is held in `pending` until it
        // reaches BATCH — minutes of viewer lag at low event rates.
        if last_flush.elapsed() >= FLUSH_INTERVAL && (!pending.is_empty() || !pending_gc.is_empty())
        {
            if !pending.is_empty() {
                db.ingest_executions(std::mem::take(&mut pending));
            }
            if !pending_gc.is_empty() {
                db.ingest_gc(std::mem::take(&mut pending_gc));
            }
            last_flush = Instant::now();
        }

        // Read more raw bytes (blocking up to the read timeout).
        match stream.read(&mut tmp) {
            Ok(0) => {
                // Leftover bytes the decoder couldn't turn into a frame mean the
                // stream ended mid-frame (target killed / socket torn) — flag it so
                // a truncated tail isn't mistaken for a clean detach.
                if !buf.is_empty() {
                    warn!(
                        bytes = buf.len(),
                        "target closed mid-frame; dropping a partial trailing event"
                    );
                } else {
                    info!("target closed the connection");
                }
                break;
            }
            Ok(n) => {
                // Count the raw stream volume processed (viewer header stat).
                db.add_bytes(n);
                buf.extend_from_slice(&tmp[..n]);
            }
            // Idle: flush buffered events so the live viewer sees them promptly.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !pending.is_empty() {
                    db.ingest_executions(std::mem::take(&mut pending));
                }
                if !pending_gc.is_empty() {
                    db.ingest_gc(std::mem::take(&mut pending_gc));
                }
                last_flush = Instant::now();
            }
            // A real socket error (e.g. connection reset): the stream is over. Stop
            // cleanly — break to the final flush so the executions we already have are
            // preserved — rather than propagating an error that drops them.
            Err(e) => {
                warn!(error = %e, "event stream read error — stopping collector");
                break;
            }
        }
    }
    // Close each thread's still-running interval at THAT thread's last observed
    // timestamp (not a global max), so a quiet thread's final execution isn't inflated
    // by another thread's later activity. A zero-length tail is skipped.
    for (thread, (gid, start, name, func, task)) in cur.drain() {
        let dur = last_ts
            .get(&thread)
            .copied()
            .unwrap_or(start)
            .saturating_sub(start);
        if dur == 0 {
            continue;
        }
        // No closing switch ever arrived for this execution, so it has no yield-point
        // stack (the bootstrap only walks at close).
        pending.push(Execution {
            gid,
            start,
            dur,
            name,
            func,
            task,
            stack: String::new(),
        });
    }
    // Final flush of anything still buffered.
    if !pending.is_empty() {
        db.ingest_executions(pending);
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
fn open(
    file: &Path,
    addr: SocketAddr,
    web_dir: Option<PathBuf>,
    no_auth: bool,
    thresholds: Thresholds,
) -> Result<()> {
    let bytes = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    // No cap: a recording is loaded whole and queried, not endlessly appended.
    let db = Db::spawn(None)?;
    // Stream the file into the DB chunk-by-chunk (bounded memory on open).
    let pid = record::ingest_file(file, &db)?;
    info!(executions = db.total(), bytes, file = %file.display(), "loaded recording");
    // Resolve to an absolute path so the viewer can show the full location
    // (the CLI arg may be relative). Fall back to the given path if the file
    // can't be canonicalized.
    let source = Some(server::Source {
        file: std::fs::canonicalize(file)
            .unwrap_or_else(|_| file.to_path_buf())
            .display()
            .to_string(),
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
        server::serve(
            db, pid, detached, source, None, None, thresholds, addr, web_dir, no_auth, shutdown,
        )
        .await
    })?;
    Ok(())
}

// ── Headless analysis (`greenlane analyze`) ─────────────────────────────────

/// The full headless report. Serialized directly for `--format json`; rendered by
/// [`print_report_text`] for the default text output. All durations are ns.
#[derive(serde::Serialize)]
struct Report {
    pid: i32,
    file: String,
    #[serde(rename = "fileBytes")]
    file_bytes: u64,
    #[serde(rename = "totalExecutions")]
    total_executions: u64,
    /// Captured wall span (ns): last execution end − first execution start.
    #[serde(rename = "spanNs")]
    span_ns: u64,
    #[serde(rename = "epochMs")]
    epoch_ms: Option<u64>,
    greenlets: u64,
    #[serde(rename = "longMs")]
    long_ms: u64,
    #[serde(rename = "blockedMs")]
    blocked_ms: u64,
    #[serde(rename = "hubRunNs")]
    hub_run_ns: u64,
    #[serde(rename = "nonHubRunNs")]
    nonhub_run_ns: u64,
    #[serde(rename = "p50Ns")]
    p50_ns: u64,
    #[serde(rename = "p95Ns")]
    p95_ns: u64,
    #[serde(rename = "p99Ns")]
    p99_ns: u64,
    #[serde(rename = "longCount")]
    long_count: u64,
    #[serde(rename = "blockedCount")]
    blocked_count: u64,
    #[serde(rename = "gcCount")]
    gc_count: u64,
    #[serde(rename = "gcTotalNs")]
    gc_total_ns: u64,
    #[serde(rename = "gcByGen")]
    gc_by_gen: Vec<GcGen>,
    slowest: Vec<SlowRow>,
    #[serde(rename = "topGreenlets")]
    top_greenlets: Vec<GreenletAgg>,
    #[serde(rename = "topFuncs")]
    top_funcs: Vec<FuncAgg>,
}

/// Load a `.glr` recording and print a headless report. Reuses the same DB + query
/// path as the viewer (no browser, no server) so triage works in a terminal / CI.
fn analyze(file: &Path, format: AnalyzeFormat, top: usize, thresholds: Thresholds) -> Result<()> {
    let file_bytes = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    // No cap: a recording is loaded whole and queried, not endlessly appended.
    let db = Db::spawn(None)?;
    let pid = record::ingest_file(file, &db)?;
    info!(executions = db.total(), bytes = file_bytes, file = %file.display(), "loaded recording");

    // Drive the async query API from a lightweight current-thread runtime (the DB
    // itself runs on its own OS thread; these awaits just wait on its replies).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (summary, slow, stats) = rt.block_on(async {
        let summary = db
            .query(Query::Summary {
                long_ns: thresholds.long_ns,
                blocked_ns: thresholds.blocked_ns,
                top,
            })
            .await?;
        let slow = db
            .query(Query::Slowlog {
                long_ns: thresholds.long_ns,
                blocked_ns: thresholds.blocked_ns,
                tier: SlowTier::All,
                sort_dur: true,
                limit: top,
            })
            .await?;
        let stats = db
            .query(Query::Stats {
                t0: 0,
                t1: u64::MAX >> 1,
            })
            .await?;
        anyhow::Ok((summary, slow, stats))
    })?;

    let Reply::Summary {
        greenlets,
        hub_run_ns,
        nonhub_run_ns,
        long_count,
        blocked_count,
        top_greenlets,
        top_funcs,
        gc_count,
        gc_total_ns,
        gc_by_gen,
    } = summary
    else {
        bail!("unexpected reply to Summary query");
    };
    let Reply::Slowlog { rows: slowest, .. } = slow else {
        bail!("unexpected reply to Slowlog query");
    };
    let Reply::Stats { p50, p95, p99 } = stats else {
        bail!("unexpected reply to Stats query");
    };

    let report = Report {
        pid,
        file: file.display().to_string(),
        file_bytes,
        total_executions: db.total(),
        span_ns: db.span().saturating_sub(db.origin()),
        epoch_ms: db.epoch(),
        greenlets,
        long_ms: thresholds.long_ns / 1_000_000,
        blocked_ms: thresholds.blocked_ns / 1_000_000,
        hub_run_ns,
        nonhub_run_ns,
        p50_ns: p50 as u64,
        p95_ns: p95 as u64,
        p99_ns: p99 as u64,
        long_count,
        blocked_count,
        gc_count,
        gc_total_ns,
        gc_by_gen,
        slowest,
        top_greenlets,
        top_funcs,
    };

    match format {
        AnalyzeFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        AnalyzeFormat::Text => print_report_text(&report),
    }
    Ok(())
}

/// Human-readable duration: ns → the largest sensible unit (ns/µs/ms/s).
fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.2}s", ns as f64 / 1e9)
    } else if ns >= 1_000_000 {
        format!("{:.1}ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.1}µs", ns as f64 / 1e3)
    } else {
        format!("{ns}ns")
    }
}

/// `n / total` as a percentage string (guards divide-by-zero).
fn pct(n: u64, total: u64) -> String {
    if total == 0 {
        "0.0%".to_string()
    } else {
        format!("{:.1}%", n as f64 / total as f64 * 100.0)
    }
}

/// Render the report as a sectioned text block to stdout.
fn print_report_text(r: &Report) {
    let epoch = match r.epoch_ms {
        Some(ms) => format!("{ms} (unix ms at t0)"),
        None => "unknown".to_string(),
    };
    println!("greenlane analysis — {}", r.file);
    println!(
        "  PID {} · {} on disk · {} executions · {} greenlets · span {}",
        r.pid,
        human_bytes(r.file_bytes),
        r.total_executions,
        r.greenlets,
        fmt_ns(r.span_ns),
    );
    println!("  recorded epoch: {epoch}");

    if r.total_executions == 0 {
        println!("\n  (empty recording — no executions captured)");
        return;
    }

    println!("\nWork");
    println!(
        "  application run time (non-hub): {} ({} of span)",
        fmt_ns(r.nonhub_run_ns),
        pct(r.nonhub_run_ns, r.span_ns)
    );
    println!("  hub/loop time: {}", fmt_ns(r.hub_run_ns));
    println!(
        "  execution percentiles (non-hub): p50 {} · p95 {} · p99 {}",
        fmt_ns(r.p50_ns),
        fmt_ns(r.p95_ns),
        fmt_ns(r.p99_ns),
    );

    println!("\nSlow executions (≥ long {}ms)", r.long_ms);
    println!(
        "  long: {}   blocked (≥ {}ms): {}",
        r.long_count, r.blocked_ms, r.blocked_count
    );
    if !r.slowest.is_empty() {
        println!("  slowest:");
        for s in &r.slowest {
            let tier = if s.level >= 2 { "blocked" } else { "long " };
            let func = if s.func.is_empty() {
                "(no func)"
            } else {
                &s.func
            };
            println!("    {:>9}  [{}]  {}  {}", fmt_ns(s.dur), tier, s.name, func);
        }
    }

    println!("\nGC");
    println!(
        "  {} pauses · {} total ({} of span)",
        r.gc_count,
        fmt_ns(r.gc_total_ns),
        pct(r.gc_total_ns, r.span_ns)
    );
    for g in &r.gc_by_gen {
        println!(
            "    gen {}: {} × ({} total)",
            g.generation,
            g.count,
            fmt_ns(g.total_ns)
        );
    }

    println!("\nTop greenlets by run time");
    for l in &r.top_greenlets {
        let hub = if l.is_hub { " (hub)" } else { "" };
        println!(
            "    {:>9}  {}{}  gid {} · {} executions",
            fmt_ns(l.run_ns),
            l.name,
            hub,
            l.gid,
            l.executions
        );
    }

    println!("\nHottest functions (non-hub, by total run time)");
    for f in &r.top_funcs {
        let func = if f.func.is_empty() {
            "(no func)"
        } else {
            &f.func
        };
        println!(
            "    {:>9}  {:>6}×  max {}  {}",
            fmt_ns(f.total_ns),
            f.count,
            fmt_ns(f.max_ns),
            func
        );
    }
}

/// Compact human bytes (B/KB/MB/GB) for the report header.
fn human_bytes(b: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Remove the private working directory (socket + every bootstrap file we wrote).
fn cleanup(base: &Path) {
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(test)]
mod tests {
    //! Collector latency: a steady, sub-BATCH trickle on an OPEN connection must
    //! reach the DB within ~FLUSH_INTERVAL — not be held until a 2048-event batch
    //! fills (the "60s lag, then 2048 events at once" bug). The writer feeds bytes
    //! faster than the read timeout and never closes the socket, so the idle-flush
    //! path can't fire; only the time-based flush can deliver these executions.
    use super::*;
    use crate::trace_format::Encoder;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    #[test]
    fn steady_trickle_flushes_without_filling_a_batch() {
        let (mut tx, rx) = UnixStream::pair().unwrap();
        let db = Db::spawn(None).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let detached = Arc::new(AtomicBool::new(false));
        let thr = Thresholds {
            long_ns: 20_000_000,
            blocked_ns: 50_000_000,
        };

        let reader = {
            let (running, detached, db) = (running.clone(), detached.clone(), db.clone());
            std::thread::spawn(move || {
                let _ = read_executions(rx, &running, &detached, &db, None, thr);
            })
        };

        // Header + schemas + meta once, then one switch every 8ms (< the read
        // timeout, so the reader's idle branch never fires).
        let writer = {
            let running = running.clone();
            std::thread::spawn(move || {
                let mut enc = Encoder::new();
                enc.write_wire_schemas();
                enc.meta(1700, 0, 1, "");
                if tx.write_all(enc.bytes()).is_err() {
                    return;
                }
                enc.clear_out();
                let mut ts = 1_000_000u64;
                let mut target = 0u64;
                while running.load(Ordering::SeqCst) {
                    ts += 1_000_000;
                    target += 1;
                    // Same thread (7), rising target → each switch closes the prior
                    // target's interval into a execution.
                    enc.switch(ts, target, "Greenlet-1", "", "", "", 7);
                    if tx.write_all(enc.bytes()).is_err() {
                        break;
                    }
                    let _ = tx.flush();
                    enc.clear_out();
                    std::thread::sleep(Duration::from_millis(8));
                }
            })
        };

        // Within a generous window the DB must have received executions, despite far
        // fewer than BATCH (2048) being sent and the connection staying open.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut total = 0;
        while Instant::now() < deadline {
            total = db.total();
            if total >= 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        running.store(false, Ordering::SeqCst);
        let _ = reader.join();
        let _ = writer.join();

        assert!(
            total >= 3,
            "a steady trickle should flush within ~FLUSH_INTERVAL; got {total} executions"
        );
        assert!(
            total < 2048,
            "executions arrived only after a full BATCH — the latency bug ({total})"
        );
    }
}
