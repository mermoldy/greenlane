//! greenlane — attach to a running gevent process and profile greenlet switches.
//!
//! Approach:
//!   1. Bind a Unix STREAM socket.
//!   2. Inject `bootstrap.py` into the target via `sys.remote_exec` (PEP 768,
//!      CPython 3.14+). The bootstrap registers a `greenlet.settrace` hook and
//!      streams switch events back to us over that socket.
//!   3. Either print a live CLI summary, or (`--serve`) feed the events into a
//!      timeline store and serve a live web viewer over HTTP.

mod server;
mod store;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use store::{GcEvent, Slice, Store};

/// bootstrap.py with a `__SOCKET_PATH__` placeholder we substitute at runtime.
const BOOTSTRAP_TEMPLATE: &str = include_str!("bootstrap.py");

#[derive(Parser)]
#[command(
    name = "greenlane",
    about = "Attach to a running gevent process and profile greenlet switches"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Attach to a running Python/gevent process by PID.
    Attach {
        /// Target process PID.
        pid: i32,
        /// Python interpreter used to trigger `sys.remote_exec` (must be 3.14+).
        #[arg(long, default_value = "python3")]
        python: String,
        /// Seconds between live summary prints (CLI mode only).
        #[arg(long, default_value_t = 2)]
        interval: u64,
        /// Skip sys.remote_exec; just listen and print the bootstrap to load
        /// manually. Use on hosts where remote attach is blocked (e.g. macOS
        /// task-port restrictions) and you self-instrument your app instead.
        #[arg(long)]
        no_inject: bool,
        /// Serve the live web viewer at this address instead of printing to the
        /// terminal, e.g. `127.0.0.1:8080`. Bind to localhost unless you mean
        /// to expose the profiler on the network.
        #[arg(long)]
        serve: Option<SocketAddr>,
        /// Serve viewer assets from this directory instead of the ones embedded
        /// in the binary (for frontend iteration).
        #[arg(long)]
        web_dir: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Attach {
            pid,
            python,
            interval,
            no_inject,
            serve,
            web_dir,
        } => attach(pid, &python, interval, no_inject, serve, web_dir),
    }
}

fn attach(
    pid: i32,
    python: &str,
    interval: u64,
    no_inject: bool,
    serve: Option<SocketAddr>,
    web_dir: Option<PathBuf>,
) -> Result<()> {
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
    println!("greenlane: listening on {}", sock_path.display());

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .context("installing SIGINT handler")?;
    }

    let bootstrap_path = write_bootstrap(pid, &base, &sock_path)?;
    if no_inject {
        println!(
            "greenlane: --no-inject — load this into the target yourself, e.g.\n    \
             exec(open({:?}).read())\nwaiting for connection…",
            bootstrap_path.to_string_lossy()
        );
    } else {
        inject(python, pid, &bootstrap_path)?;
        println!("greenlane: injected bootstrap into pid {pid}, waiting for connection…");
    }

    // Wait for the target to connect back (it runs remote_exec at its next safe point).
    let stream = match accept(&listener, &running)? {
        Some(s) => s,
        None => {
            cleanup(&sock_path, &bootstrap_path);
            return Ok(());
        }
    };

    let result = match serve {
        Some(addr) => {
            println!("greenlane: connected — streaming to viewer (Ctrl-C to stop)");
            serve_mode(stream, running.clone(), pid, addr, web_dir)
        }
        None => {
            println!("greenlane: connected — collecting greenlet switches (Ctrl-C to stop)\n");
            collect(stream, &running, interval)
        }
    };
    cleanup(&sock_path, &bootstrap_path);
    result
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
            "sys.remote_exec failed (exit {:?}).\n\
             Common causes: target/helper Python < 3.14; insufficient privileges \
             (try sudo, or matching uid); remote debugging disabled \
             (PYTHON_DISABLE_REMOTE_DEBUG / -X disable_remote_debug).\n\
             stderr:\n{stderr}",
            out.status.code()
        );
    }
    Ok(())
}

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

/// One greenlet switch parsed from the event stream.
struct SwitchEvent {
    ts: u64,
    throw: bool,
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
    let event = parts.next()?;
    let _origin = parts.next()?;
    let target = parts.next()?.parse::<u64>().ok()?;
    let label = parts.next().unwrap_or("").to_string();
    let func = parts.next().unwrap_or("").to_string();
    let task = parts.next().unwrap_or("").to_string();
    let stack = parts.next().unwrap_or("").to_string();
    Some(SwitchEvent {
        ts,
        throw: event == "throw",
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
) -> Result<()> {
    let store = Store::new();
    // Flipped by POST /detach: stops the collector so the target self-removes
    // its trace hook (the broken socket triggers cleanup in the bootstrap).
    let detached = Arc::new(AtomicBool::new(false));

    // The blocking socket reader stays on its own std thread, feeding the store.
    let collector = {
        let running = running.clone();
        let detached = detached.clone();
        let store = store.clone();
        std::thread::spawn(move || {
            if let Err(e) = read_slices(stream, &running, &detached, &store) {
                eprintln!("greenlane: collector error: {e:#}");
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
        server::serve(store, pid, detached.clone(), addr, web_dir, shutdown).await
    })?;

    running.store(false, Ordering::SeqCst);
    let _ = collector.join();
    Ok(())
}

/// Read the switch stream, closing one [`Slice`] per run-interval into the store.
/// A greenlet's slice closes when the *next* switch arrives (same attribution as
/// the CLI summary): time from being switched-in until being switched-away.
fn read_slices(
    stream: UnixStream,
    running: &AtomicBool,
    detached: &AtomicBool,
    store: &Store,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // (gid, start_ns, name, func, task, stack) for the currently-running
    // greenlet — captured when it was switched in (its resume point).
    let mut cur: Option<(u64, u64, String, String, String, String)> = None;

    while running.load(Ordering::SeqCst) && !detached.load(Ordering::SeqCst) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                println!("greenlane: target closed the connection");
                break;
            }
            Ok(_) => {
                let trimmed = line.trim_end();
                // Header line: "meta\t<epoch_ms>" — wall-clock at trace t0.
                if let Some(rest) = trimmed.strip_prefix("meta\t") {
                    if let Ok(ms) = rest.parse::<u64>() {
                        store.set_epoch(ms);
                    }
                    continue;
                }
                // GC pause: "gc\t<start_ns>\t<dur_ns>\t<gen>\t<collected>".
                if let Some(ev) = trimmed.strip_prefix("gc\t").and_then(parse_gc) {
                    store.push_gc(ev);
                    continue;
                }
                if let Some(ev) = parse_event(trimmed) {
                    if let Some((gid, start, name, func, task, stack)) = cur.take() {
                        store.push(Slice {
                            gid,
                            start,
                            dur: ev.ts.saturating_sub(start),
                            name,
                            func,
                            task,
                            stack,
                        });
                    }
                    cur = Some((ev.target, ev.ts, ev.label, ev.func, ev.task, ev.stack));
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e).context("reading event stream"),
        }
    }
    Ok(())
}

// ── CLI summary mode ───────────────────────────────────────────────────────

/// Per-greenlet aggregation derived from the switch stream.
#[derive(Default)]
struct Stats {
    total: u64,
    throws: u64,
    /// Greenlet currently running (last switch target) and when it began.
    current: Option<u64>,
    last_ts_ns: Option<u64>,
    /// id -> nanoseconds spent running.
    run_ns: HashMap<u64, u64>,
    /// id -> times switched into.
    switches_in: HashMap<u64, u64>,
    /// id -> human label (type name + minimal_ident), e.g. "Hub", "Greenlet-3".
    ident: HashMap<u64, String>,
}

impl Stats {
    fn record(&mut self, ev: &SwitchEvent) {
        self.total += 1;
        if ev.throw {
            self.throws += 1;
        }
        // Attribute the elapsed slice to whoever was running until now.
        if let (Some(cur), Some(prev_ts)) = (self.current, self.last_ts_ns) {
            *self.run_ns.entry(cur).or_default() += ev.ts.saturating_sub(prev_ts);
        }
        self.current = Some(ev.target);
        self.last_ts_ns = Some(ev.ts);
        *self.switches_in.entry(ev.target).or_default() += 1;
        if !ev.label.is_empty() {
            self.ident
                .entry(ev.target)
                .or_insert_with(|| ev.label.clone());
        }
    }

    fn print_summary(&self, wall: Duration) {
        let secs = wall.as_secs_f64().max(1e-9);
        println!(
            "── {} switches ({} throws) in {:.1}s · {:.0}/s · {} greenlets seen",
            self.total,
            self.throws,
            secs,
            self.total as f64 / secs,
            self.switches_in.len(),
        );
        let mut top: Vec<_> = self.run_ns.iter().collect();
        top.sort_by(|a, b| b.1.cmp(a.1));
        for (id, ns) in top.into_iter().take(10) {
            let label = self
                .ident
                .get(id)
                .cloned()
                .unwrap_or_else(|| format!("0x{id:x}"));
            let switches = self.switches_in.get(id).copied().unwrap_or(0);
            println!(
                "   {:<14} {:>8.1} ms running  {:>7} switches-in",
                label,
                *ns as f64 / 1e6,
                switches,
            );
        }
        println!();
    }
}

fn collect(stream: UnixStream, running: &AtomicBool, interval: u64) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut reader = BufReader::new(stream);
    let mut stats = Stats::default();
    let start = Instant::now();
    let mut last_print = Instant::now();
    let mut line = String::new();

    while running.load(Ordering::SeqCst) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                println!("greenlane: target closed the connection");
                break;
            }
            Ok(_) => {
                if let Some(ev) = parse_event(line.trim_end()) {
                    stats.record(&ev);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e).context("reading event stream"),
        }
        if last_print.elapsed() >= Duration::from_secs(interval) {
            stats.print_summary(start.elapsed());
            last_print = Instant::now();
        }
    }

    println!("\n=== final profile ===");
    stats.print_summary(start.elapsed());
    Ok(())
}

fn cleanup(sock_path: &PathBuf, bootstrap_path: &PathBuf) {
    let _ = std::fs::remove_file(sock_path);
    let _ = std::fs::remove_file(bootstrap_path);
}
