"""Gevent demo app: monkey-patch everything, then run 10 greenlets with a
mix of I/O-bound and CPU-bound workloads, each doing random blocking.

Run with:  python test.py
"""

# Monkey-patch the stdlib BEFORE importing anything that does I/O.
from gevent import monkey

monkey.patch_all()

import os
import random
import time

import gevent


def log(msg):
    """Timestamped log line including the current greenlet id."""
    gid = id(gevent.getcurrent()) & 0xFFFF
    print(f"[{time.strftime('%H:%M:%S')}] [g-{gid:04x}] {msg}", flush=True)


def io_bound(worker_id):
    """Simulate I/O work: cooperative sleeps that yield to other greenlets."""
    rounds = random.randint(3, 6)
    log(f"worker {worker_id} (IO) starting, {rounds} rounds")
    for r in range(rounds):
        # gevent.sleep yields the hub so other greenlets can run.
        delay = random.uniform(0.1, 0.8)
        log(f"worker {worker_id} (IO) round {r}: blocking {delay:.2f}s")
        time.sleep(delay)  # patched -> cooperative
    log(f"worker {worker_id} (IO) done")
    return ("io", worker_id, rounds)


def cpu_bound(worker_id):
    """Simulate CPU work: a tight loop that occasionally yields so the
    event loop is not starved. Real CPU work blocks the hub, so we sleep(0)
    periodically to keep things cooperative."""
    iterations = random.randint(2, 5)
    log(f"worker {worker_id} (CPU) starting, {iterations} chunks")
    total = 0
    for c in range(iterations):
        n = random.randint(200_000, 600_000)
        log(f"worker {worker_id} (CPU) chunk {c}: crunching {n} ints")
        for i in range(n):
            total += i * i
        # Random blocking sleep mixed in with the CPU work.
        gevent.sleep(random.uniform(0.0, 0.3))
    log(f"worker {worker_id} (CPU) done, total={total}")
    return ("cpu", worker_id, total)


def worker(worker_id):
    """Randomly pick an I/O-bound or CPU-bound workload."""
    if random.random() < 0.5:
        return io_bound(worker_id)
    return cpu_bound(worker_id)


def heartbeat(interval=5.0):
    """Background greenlet: periodically log the process PID so you can see
    the app is alive and which process to signal."""
    while True:
        log(f"[heartbeat] alive -- PID={os.getpid()}")
        gevent.sleep(interval)


def run_batch(batch):
    """Spawn 10 greenlets, run them to completion, report a summary."""
    log(f"=== batch {batch}: spawning 10 greenlets ===")
    start = time.time()

    greenlets = [gevent.spawn(worker, i) for i in range(10)]
    gevent.joinall(greenlets)

    elapsed = time.time() - start
    io_count = sum(1 for g in greenlets if g.value and g.value[0] == "io")
    cpu_count = sum(1 for g in greenlets if g.value and g.value[0] == "cpu")
    log(f"=== batch {batch} done in {elapsed:.2f}s  (io={io_count}, cpu={cpu_count}) ===")


def main():
    random.seed(os.getpid())
    log(f"starting up -- PID={os.getpid()}")
    # Background greenlet that logs the PID every few seconds.
    hb = gevent.spawn(heartbeat, 5.0)
    batch = 0
    try:
        while True:
            run_batch(batch)
            batch += 1
            # Brief pause between batches so output stays readable.
            gevent.sleep(random.uniform(0.2, 1.0))
    except KeyboardInterrupt:
        log(f"Ctrl+C received -- stopping after {batch} batches. Bye!")
    finally:
        hb.kill()


if __name__ == "__main__":
    main()
