//! Host / process / runtime introspection and kernel scheduler-lag sampling.
//!
//! Two jobs:
//!
//!   1. **Static-ish facts** for the viewer's System panel: host kernel (uname),
//!      CPU count, the target's cgroup CPU limit, and live `/proc/<pid>/status`
//!      fields. Python/runtime/thread facts are reported by the bootstrap itself
//!      (it knows the interpreter best) and stashed here as opaque JSON.
//!
//!   2. **Kernel scheduler lag.** The bootstrap reports the runtime thread's OS
//!      tid; we then read `/proc/<pid>/task/<tid>/schedstat` from *outside* the
//!      target, so the measurement never perturbs the hot path or touches the
//!      GIL. Field 2 of schedstat is the cumulative nanoseconds the thread was
//!      runnable but *not on a CPU* — i.e. scheduler lag. We also pick up cgroup
//!      CFS throttling (the big one in k8s: the kernel deliberately descheduling
//!      you at the quota) and CPU pressure (PSI) while we're in `/proc`.
//!
//! All of this is Linux-only; on other platforms the lag snapshot stays `None`
//! and only uname/CPU count are reported.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How often the sampler polls `/proc` for scheduler-lag deltas.
const SAMPLE_EVERY: Duration = Duration::from_millis(100);

/// Everything the `/info` endpoint serves. Cheaply clonable via `Arc`.
pub struct SysInfo {
    pid: i32,
    /// Host kernel + CPU + cgroup limit — gathered once at startup.
    kernel: Value,
    /// `/proc/<pid>/status` snapshot — refreshed by the sampler.
    process: Mutex<Value>,
    /// Python/runtime/thread facts reported by the bootstrap (opaque passthrough).
    pyinfo: Mutex<Option<Value>>,
    /// Runtime (hub/loop) thread id reported by the bootstrap; 0 until known.
    tid: AtomicU64,
    /// Latest scheduler-lag snapshot (`None` until the sampler runs / unsupported).
    lag: Mutex<Option<Value>>,
    /// Ensures we only ever start one sampler even if `tid` is reported twice.
    sampler_started: AtomicBool,
}

impl SysInfo {
    pub fn new(pid: i32) -> Arc<Self> {
        Arc::new(Self {
            pid,
            kernel: gather_kernel(pid),
            process: Mutex::new(gather_process(pid)),
            pyinfo: Mutex::new(None),
            tid: AtomicU64::new(0),
            lag: Mutex::new(None),
            sampler_started: AtomicBool::new(false),
        })
    }

    /// Store the Python/runtime facts the bootstrap sent (a JSON object).
    pub fn set_pyinfo(&self, raw: &str) {
        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            *self.pyinfo.lock().unwrap() = Some(v);
        }
    }

    /// Record the runtime thread id and, the first time, start the lag sampler.
    pub fn set_tid(self: &Arc<Self>, tid: u64, running: Arc<AtomicBool>) {
        self.tid.store(tid, Ordering::Relaxed);
        if self
            .sampler_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let this = self.clone();
            std::thread::Builder::new()
                .name("greenlane-lag".into())
                .spawn(move || sample_loop(this, tid, running))
                .ok();
        }
    }

    /// The full `/info` document for the viewer's System panel.
    pub fn to_json(&self, live: bool, source: Option<Value>) -> Value {
        json!({
            "pid": self.pid,
            "live": live,
            "source": source,
            "tid": match self.tid.load(Ordering::Relaxed) { 0 => Value::Null, t => json!(t) },
            "kernel": self.kernel,
            "process": *self.process.lock().unwrap(),
            "python": *self.pyinfo.lock().unwrap(),
            "lag": *self.lag.lock().unwrap(),
        })
    }

    /// Latest hub-thread run-queue delay rate ("ms of CPU starvation per wall
    /// second"), or `None` when schedstat is unavailable (non-Linux / no
    /// CONFIG_SCHEDSTATS / not yet sampled). Drives R13's timeline lag band: the
    /// server tags each live `head` with this, and the viewer plots it at the
    /// current live edge — so it aligns to the trace axis with no clock mapping.
    pub fn lag_rate_ms_s(&self) -> Option<f64> {
        let g = self.lag.lock().unwrap();
        g.as_ref()?.get("runqRateMsPerSec")?.as_f64()
    }

    /// Latest hub-thread on-CPU rate ("ms on CPU per wall-second"; /1000 = utilization
    /// fraction), or `None` when schedstat is unavailable. Tagged onto each live `head`
    /// so the viewer can keep the CPU band's live tail moving in the pending area —
    /// independent of the (lagging) execution stream, exactly like the lag band.
    pub fn cpu_rate_ms_s(&self) -> Option<f64> {
        let g = self.lag.lock().unwrap();
        g.as_ref()?.get("onCpuRateMsPerSec")?.as_f64()
    }
}

// ── one-shot host/process gathering ──────────────────────────────────────────

fn gather_kernel(pid: i32) -> Value {
    let mut v = json!({
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
    });
    if let Some((os, release, version, machine)) = uname() {
        v["os"] = json!(os);
        v["release"] = json!(release);
        v["version"] = json!(version);
        v["machine"] = json!(machine);
    }
    if let Some(cores) = cgroup_quota_cores(pid) {
        v["cgroupQuotaCores"] = json!(cores);
    }
    v
}

/// `/proc/<pid>/status` fields the panel shows (Linux only).
fn gather_process(pid: i32) -> Value {
    let mut v = json!({});
    let path = format!("/proc/{pid}/status");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return v;
    };
    for line in text.lines() {
        let Some((k, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        match k {
            "Name" => v["name"] = json!(val),
            "State" => v["state"] = json!(val),
            "Threads" => v["threads"] = json!(val.parse::<u64>().unwrap_or(0)),
            "VmRSS" => v["rssKb"] = json!(kb(val)),
            "VmPeak" => v["vmPeakKb"] = json!(kb(val)),
            "voluntary_ctxt_switches" => {
                v["voluntaryCtxt"] = json!(val.parse::<u64>().unwrap_or(0))
            }
            "nonvoluntary_ctxt_switches" => {
                v["involuntaryCtxt"] = json!(val.parse::<u64>().unwrap_or(0))
            }
            _ => {}
        }
    }
    v
}

/// Parse a "1234 kB" status value to a number of kB.
fn kb(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn uname() -> Option<(String, String, String, String)> {
    // Safe: uname fills the struct; we read NUL-terminated C strings out of it.
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) != 0 {
            return None;
        }
        Some((
            cstr(u.sysname.as_ptr()),
            cstr(u.release.as_ptr()),
            cstr(u.version.as_ptr()),
            cstr(u.machine.as_ptr()),
        ))
    }
}

unsafe fn cstr(p: *const libc::c_char) -> String {
    unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
}

// ── cgroup (v2) ──────────────────────────────────────────────────────────────

/// The target's cgroup-v2 path under the unified mount, e.g. `/sys/fs/cgroup/...`.
fn cgroup_base(pid: i32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    // cgroup v2: a single line `0::<path>`.
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            return Some(format!("/sys/fs/cgroup{}", rest.trim_end_matches('/')));
        }
    }
    None
}

/// CFS quota expressed in whole CPUs (e.g. `1.5`), or `None` if unlimited.
fn cgroup_quota_cores(pid: i32) -> Option<f64> {
    let base = cgroup_base(pid)?;
    let raw = std::fs::read_to_string(format!("{base}/cpu.max")).ok()?;
    let mut it = raw.split_whitespace();
    let quota = it.next()?;
    if quota == "max" {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    let period: f64 = it.next().and_then(|p| p.parse().ok()).unwrap_or(100_000.0);
    (period > 0.0).then(|| quota / period)
}

// ── scheduler-lag sampler ────────────────────────────────────────────────────

/// Read `<a> <b> <c>` from a thread's schedstat: (on-cpu ns, runqueue-wait ns).
fn read_schedstat(pid: i32, tid: u64) -> Option<(u64, u64)> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/schedstat")).ok()?;
    let mut it = raw.split_whitespace();
    let on_cpu = it.next()?.parse().ok()?;
    let runq = it.next()?.parse().ok()?;
    Some((on_cpu, runq))
}

/// Parse cgroup `cpu.stat`: (nr_periods, nr_throttled, throttled_usec).
fn read_throttle(base: &str) -> Option<(u64, u64, u64)> {
    let raw = std::fs::read_to_string(format!("{base}/cpu.stat")).ok()?;
    let (mut periods, mut throttled, mut usec) = (0, 0, 0);
    for line in raw.lines() {
        let mut it = line.split_whitespace();
        match (it.next(), it.next().and_then(|n| n.parse::<u64>().ok())) {
            (Some("nr_periods"), Some(n)) => periods = n,
            (Some("nr_throttled"), Some(n)) => throttled = n,
            (Some("throttled_usec"), Some(n)) => usec = n,
            _ => {}
        }
    }
    Some((periods, throttled, usec))
}

/// Parse the `some avg10=…` field from a PSI `cpu.pressure` file.
fn read_psi_some_avg10(base: &str) -> Option<f64> {
    let raw = std::fs::read_to_string(format!("{base}/cpu.pressure")).ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            for tok in rest.split_whitespace() {
                if let Some(v) = tok.strip_prefix("avg10=") {
                    return v.parse().ok();
                }
            }
        }
    }
    None
}

/// Poll `/proc` for this thread's run-queue delay (and cgroup throttling / PSI)
/// until the session ends, updating the shared `lag` snapshot each tick.
fn sample_loop(sys: Arc<SysInfo>, tid: u64, running: Arc<AtomicBool>) {
    let pid = sys.pid;
    let base = cgroup_base(pid);
    let mut prev = read_schedstat(pid, tid);
    let mut prev_t = Instant::now();

    while running.load(Ordering::SeqCst) {
        std::thread::sleep(SAMPLE_EVERY);
        let now = Instant::now();
        let dt = now.duration_since(prev_t).as_secs_f64().max(1e-3);

        let Some((on_cpu, runq)) = read_schedstat(pid, tid) else {
            // schedstat unavailable (no CONFIG_SCHEDSTATS / not Linux): report so
            // the UI can say so, then keep idling cheaply.
            *sys.lag.lock().unwrap() = Some(json!({ "available": false }));
            continue;
        };

        // Deltas since the last sample → rates in "ms per wall-second": run-queue
        // wait (CPU starvation) and on-CPU time (utilization; /1000 = fraction). The
        // on-CPU rate is a live, execution-stream-independent measure of the hub
        // thread's CPU use — so the viewer can keep the CPU band moving in the
        // pending area (ahead of the lagging trace data), the way lag already does.
        let (rate_ms_s, cpu_ms_s) = match prev {
            Some((prev_on, prev_runq)) => (
                (runq.saturating_sub(prev_runq) as f64 / 1e6) / dt,
                (on_cpu.saturating_sub(prev_on) as f64 / 1e6) / dt,
            ),
            None => (0.0, 0.0),
        };
        prev = Some((on_cpu, runq));
        prev_t = now;

        let mut lag = json!({
            "available": true,
            "runqWaitMs": runq as f64 / 1e6,
            "runqRateMsPerSec": rate_ms_s,
            "onCpuMs": on_cpu as f64 / 1e6,
            "onCpuRateMsPerSec": cpu_ms_s,
        });
        if let Some(base) = &base {
            if let Some((periods, throttled, usec)) = read_throttle(base) {
                lag["throttle"] = json!({
                    "periods": periods,
                    "throttled": throttled,
                    "throttledMs": usec as f64 / 1e3,
                });
            }
            if let Some(avg10) = read_psi_some_avg10(base) {
                lag["psiSomeAvg10"] = json!(avg10);
            }
        }
        *sys.lag.lock().unwrap() = Some(lag);

        // Keep the process panel (RSS, threads, ctxt switches) fresh too.
        *sys.process.lock().unwrap() = gather_process(pid);
    }
}
