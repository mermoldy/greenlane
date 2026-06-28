"""Asyncio demo / load generator for greenlane — mirror of ``gevent_app.py``.

Drives a bounded set of tasks through the same randomised workload so the
timeline shows a realistic mix: most jobs are fast cooperative I/O, a fraction
are heavier CPU-bound bursts (which briefly block the loop), and a rare few are
very slow. Used to exercise greenlane's asyncio bootstrap.

Both knobs are parametrised:

    python tests/asyncio_app.py                    # 50 coroutines, run forever
    python tests/asyncio_app.py -c 5000            # 5000 concurrent coroutines
    python tests/asyncio_app.py -c 200 -n 100000   # stop after 100k jobs

`-c/--concurrency` is how many task coroutines are alive at once; `-n/--events`
is the total number of jobs to run (0 = forever). Attach greenlane to the
printed PID to watch.
"""

import argparse
import asyncio
import os
import random
import time

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


_completed = [0]


async def workload(i):
    """One job. Randomly picks a profile so the timeline stays varied."""
    r = random.random()
    if r < 0.004:
        # Very rare: a pathological stall — a long, cooperative I/O block.
        await asyncio.sleep(random.uniform(0.5, 3.0))
        return "very_slow"
    if r < 0.03:
        # Small slice: a slow request, up to ~300 ms.
        await asyncio.sleep(random.uniform(0.10, 0.30))
        return "slow"
    if r < 0.12:
        # Heavier CPU-bound burst, yielding between chunks. Each chunk runs
        # uninterrupted, so it blocks the loop and shows as a fat span.
        total = 0
        for _ in range(random.randint(1, 4)):
            for j in range(random.randint(200_000, 800_000)):
                total += j * j
            await asyncio.sleep(0)
        return "cpu"
    # The common case: fast I/O.
    await asyncio.sleep(random.uniform(0.0005, 0.025))
    return "io"


async def report(concurrency, events):
    """Background task: log the job completion rate once a second."""
    last_n, last_t = _completed[0], time.perf_counter()
    log("started", coroutines=concurrency, events=events or "inf")
    while True:
        await asyncio.sleep(1.0)
        now_n, now_t = _completed[0], time.perf_counter()
        rate = (now_n - last_n) / (now_t - last_t)
        log("rate", jobs_per_sec=round(rate), total_jobs=now_n)
        last_n, last_t = now_n, now_t


async def main():
    ap = argparse.ArgumentParser(description="asyncio load generator for greenlane")
    ap.add_argument(
        "-c",
        "--concurrency",
        type=int,
        default=50,
        help="task coroutines alive at once (default: 50)",
    )
    ap.add_argument(
        "-n", "--events", type=int, default=0, help="total jobs to run, 0 = forever (default: 0)"
    )
    ap.add_argument(
        "--seed", type=int, default=None, help="RNG seed for reproducible runs (default: PID)"
    )
    args = ap.parse_args()
    random.seed(args.seed if args.seed is not None else os.getpid())

    asyncio.create_task(report(args.concurrency, args.events), name="report")

    # A semaphore bounds in-flight jobs to `-c`; the producer waits on a free
    # slot before creating the next task, so each job is its own task (its own
    # timeline lane) without the pending set growing without bound.
    sem = asyncio.Semaphore(args.concurrency)
    pending = set()

    async def job(i):
        try:
            await workload(i)
        finally:
            _completed[0] += 1
            sem.release()

    i = 0
    try:
        while args.events == 0 or i < args.events:
            await sem.acquire()
            t = asyncio.create_task(job(i), name=f"job-{i}")
            pending.add(t)
            t.add_done_callback(pending.discard)
            i += 1
        await asyncio.gather(*pending)
        log("done", total_jobs=_completed[0])
    except asyncio.CancelledError:
        log("stopping", total_jobs=_completed[0])


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
