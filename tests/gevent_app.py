"""Gevent demo app: monkey-patch everything, then run 10 greenlets with a
mix of I/O-bound and CPU-bound workloads, each doing random blocking.

Run with:  python tests/gevent_app.py
"""

# Monkey-patch the stdlib BEFORE importing anything that does I/O.
from gevent import monkey

monkey.patch_all()

import logging
import os
import random
import sys
import time

import gevent
import structlog

logging.basicConfig(format="%(message)s", stream=sys.stdout, level=logging.INFO)
structlog.configure(
    processors=[
        structlog.stdlib.add_log_level,
        structlog.processors.TimeStamper(fmt="%H:%M:%S"),
        structlog.dev.ConsoleRenderer(),
    ],
    logger_factory=structlog.stdlib.LoggerFactory(),
    wrapper_class=structlog.stdlib.BoundLogger,
)

log = structlog.get_logger()


def io_bound(worker_id):
    """Simulate I/O work: cooperative sleeps that yield to other greenlets."""
    rounds = random.randint(3, 6)
    for _ in range(rounds):
        # time.sleep is patched -> cooperative, yields the hub.
        time.sleep(random.uniform(0.1, 0.8))
    log.info("worker done", worker=worker_id, kind="io", rounds=rounds)
    return ("io", worker_id, rounds)


def cpu_bound(worker_id):
    """Simulate CPU work: a tight loop that occasionally yields so the
    event loop is not starved. Real CPU work blocks the hub, so we sleep(0)
    periodically to keep things cooperative."""
    iterations = random.randint(2, 5)
    total = 0
    for _ in range(iterations):
        n = random.randint(200_000, 600_000)
        for i in range(n):
            total += i * i
        # Random blocking sleep mixed in with the CPU work.
        gevent.sleep(random.uniform(0.0, 0.3))
    log.info("worker done", worker=worker_id, kind="cpu", chunks=iterations)
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
        log.info("heartbeat", pid=os.getpid())
        gevent.sleep(interval)


def run_batch(batch):
    """Spawn 10 greenlets, run them to completion, report a summary."""
    start = time.time()

    greenlets = [gevent.spawn(worker, i) for i in range(10)]
    gevent.joinall(greenlets)

    elapsed = time.time() - start
    io_count = sum(1 for g in greenlets if g.value and g.value[0] == "io")
    cpu_count = sum(1 for g in greenlets if g.value and g.value[0] == "cpu")
    log.info(
        "batch done", batch=batch, elapsed=round(elapsed, 2), io=io_count, cpu=cpu_count
    )


def main():
    random.seed(os.getpid())
    log.info("starting up", pid=os.getpid())
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
        log.info("stopping", batches=batch)
    finally:
        hb.kill()


if __name__ == "__main__":
    main()
