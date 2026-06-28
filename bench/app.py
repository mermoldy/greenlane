"""Gevent demo / load generator for greenlane.

Drives a pool of greenlets through a randomised workload so the timeline shows a
realistic mix: most jobs are fast cooperative I/O, a fraction are heavier
CPU-bound bursts, and a rare few are very slow — exactly the shape the slow-log
and span highlights are built to surface.

Both knobs are parametrised:

    python bench/app.py                    # 50 greenlets, run forever
    python bench/app.py -c 5000            # 5000 concurrent greenlets
    python bench/app.py -c 200 -n 100000   # stop after 100k jobs

`-c/--concurrency` is how many greenlets are alive at once; `-n/--events` is the
total number of jobs to run (0 = forever). Crank both up to stress greenlane;
attach it to the printed PID to watch.
"""

# Monkey-patch the stdlib BEFORE importing anything that does I/O.
from gevent import monkey

monkey.patch_all()

import argparse
import os
import random
import signal
import time

import gevent
import greenlet
from gevent.event import Event
from gevent.pool import Pool

# Bind the PID once as a global context label so every log line carries it.
_PID = os.getpid()

try:  # structured logging if available, else a plain timestamped fallback
    import structlog

    _log = structlog.get_logger().bind(pid=_PID)

    def log(event, **kw):
        _log.info(event, **kw)
except ImportError:

    def log(event, **kw):
        kv = " ".join(f"{k}={v}" for k, v in kw.items())
        print(
            f"[{time.strftime('%H:%M:%S')}] pid={_PID} {event}" + (f"  {kv}" if kv else ""),
            flush=True,
        )


# Count greenlet switches with the same mechanism greenlane uses, so the
# reported rate reflects the real event volume — and keeps counting after
# greenlane attaches (greenlane chains to this pre-existing hook).
_switches = [0]


def _count(event, args):
    _switches[0] += 1


# ── Deep call stacks ─────────────────────────────────────────────────────────
# A chain of distinctly-named frames so a captured stack reads like a real
# application call path (request → auth → dispatch → controller → db → …) rather
# than one repeated function. `descend` then adds a randomised number of extra
# frames, so the trace panel shows tall, varied stacks — handy for exercising
# `--include-traces` and the stack viewer. The cooperative yield happens at the
# very bottom, so the switch greenlane records carries the whole chain.
def handle_request(work):
    return authenticate(work)


def authenticate(work):
    return load_session(work)


def load_session(work):
    return dispatch(work)


def dispatch(work):
    return run_controller(work)


def run_controller(work):
    return query_database(work)


def query_database(work):
    return render_template(work)


def render_template(work):
    return descend(work, random.randint(12, 32))


def descend(work, n):
    """Recurse `n` more frames, then run the work at the bottom of the stack."""
    if n > 0:
        return descend(work, n - 1)
    return serialize(work)


def serialize(work):
    return work()


def _deep_leaf():
    """Bottom of a deep stack: a little CPU, then a cooperative yield — so the
    span is real work and the switch is sampled while the stack is tall."""
    total = 0
    for j in range(random.randint(50_000, 150_000)):
        total += j * j
    gevent.sleep(random.uniform(0.002, 0.04))
    return total


def workload(i):
    """One job. Randomly picks a profile so the timeline stays varied."""
    r = random.random()
    if r < 0.004:
        # Very rare: a pathological stall — a long, cooperative I/O block.
        gevent.sleep(random.uniform(0.5, 3.0))
        return "very_slow"
    if r < 0.03:
        # Small slice: a slow request, up to ~300 ms.
        gevent.sleep(random.uniform(0.10, 0.30))
        return "slow"
    if r < 0.12:
        # Heavier CPU-bound burst, yielding between chunks so it stays
        # cooperative but each chunk shows as a fat (often highlighted) span.
        total = 0
        for _ in range(random.randint(1, 4)):
            for j in range(random.randint(200_000, 800_000)):
                total += j * j
            gevent.sleep(0)
        return "cpu"
    if r < 0.20:
        # Deep call chain: tall trace stacks for the stack viewer / --include-traces.
        handle_request(_deep_leaf)
        return "deep"
    # The common case: fast I/O.
    gevent.sleep(random.uniform(0.0005, 0.025))
    return "io"


def report(concurrency, events):
    """Background greenlet: log the switch rate once a second."""
    last_n, last_t = _switches[0], time.perf_counter()
    log("started", greenlets=concurrency, events=events or "inf")
    while True:
        gevent.sleep(1.0)
        now_n, now_t = _switches[0], time.perf_counter()
        rate = (now_n - last_n) / (now_t - last_t)
        log("rate", switches_per_sec=round(rate), total_switches=now_n)
        last_n, last_t = now_n, now_t


def main():
    ap = argparse.ArgumentParser(description="gevent load generator for greenlane")
    ap.add_argument(
        "-c", "--concurrency", type=int, default=50, help="greenlets alive at once (default: 50)"
    )
    ap.add_argument(
        "-n", "--events", type=int, default=0, help="total jobs to run, 0 = forever (default: 0)"
    )
    ap.add_argument(
        "--seed", type=int, default=None, help="RNG seed for reproducible runs (default: PID)"
    )
    args = ap.parse_args()
    random.seed(args.seed if args.seed is not None else os.getpid())

    greenlet.settrace(_count)
    gevent.spawn(report, args.concurrency, args.events)

    # gevent raises SIGINT as a KeyboardInterrupt in whatever greenlet is running
    # at the time — usually a worker, not `main` — so a bare Ctrl+C tends to kill
    # one random job while the spawn loop keeps going. Handle the signal in the
    # hub instead, so a single Ctrl+C always stops the whole generator.
    stop = Event()
    gevent.signal_handler(signal.SIGINT, stop.set)

    # Pool.spawn blocks cooperatively when the pool is full, so this both bounds
    # concurrency to `-c` and keeps it saturated.
    pool = Pool(args.concurrency)

    def driver():
        i = 0
        while (args.events == 0 or i < args.events) and not stop.is_set():
            pool.spawn(workload, i)
            i += 1
        pool.join()

    drv = gevent.spawn(driver)
    waiter = gevent.spawn(stop.wait)
    # Return as soon as either the run completes (bounded `-n`) or Ctrl+C fires.
    gevent.joinall([drv, waiter], count=1)

    interrupted = stop.is_set()
    pool.kill(block=True)
    drv.kill(block=True)
    waiter.kill(block=True)
    log("stopping" if interrupted else "done", total_switches=_switches[0])


if __name__ == "__main__":
    main()
