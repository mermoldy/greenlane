"""Asyncio demo app: run 10 tasks with a mix of I/O-bound and CPU-bound
workloads, each doing random cooperative blocking. The asyncio analogue of
``gevent_app.py`` — used to exercise greenlane's asyncio bootstrap.

Run with:  python tests/asyncio_app.py
"""

import asyncio
import logging
import os
import random
import sys
import time

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


async def io_bound(worker_id):
    """Simulate I/O work: cooperative sleeps that yield to other tasks."""
    rounds = random.randint(3, 6)
    for _ in range(rounds):
        # asyncio.sleep yields to the event loop so other tasks can run.
        await asyncio.sleep(random.uniform(0.1, 0.8))
    log.info("worker done", worker=worker_id, kind="io", rounds=rounds)
    return ("io", worker_id, rounds)


async def cpu_bound(worker_id):
    """Simulate CPU work: a tight loop that occasionally yields so the event
    loop is not starved. Real CPU work blocks the loop, so we ``await
    asyncio.sleep(0)`` periodically to keep things cooperative."""
    iterations = random.randint(2, 5)
    total = 0
    for _ in range(iterations):
        n = random.randint(200_000, 600_000)
        for i in range(n):
            total += i * i
        # Yield to the loop, with a random extra blocking sleep mixed in.
        await asyncio.sleep(random.uniform(0.0, 0.3))
    log.info("worker done", worker=worker_id, kind="cpu", chunks=iterations)
    return ("cpu", worker_id, total)


async def worker(worker_id):
    """Randomly pick an I/O-bound or CPU-bound workload."""
    if random.random() < 0.5:
        return await io_bound(worker_id)
    return await cpu_bound(worker_id)


async def heartbeat(interval=5.0):
    """Background task: periodically log the process PID so you can see the
    app is alive and which process to attach to."""
    while True:
        log.info("heartbeat", pid=os.getpid())
        await asyncio.sleep(interval)


async def run_batch(batch):
    """Spawn 10 tasks, run them to completion, report a summary."""
    start = time.time()

    tasks = [asyncio.create_task(worker(i), name=f"worker-{i}") for i in range(10)]
    results = await asyncio.gather(*tasks)

    elapsed = time.time() - start
    io_count = sum(1 for r in results if r and r[0] == "io")
    cpu_count = sum(1 for r in results if r and r[0] == "cpu")
    log.info(
        "batch done", batch=batch, elapsed=round(elapsed, 2), io=io_count, cpu=cpu_count
    )


async def main():
    random.seed(os.getpid())
    log.info("starting up", pid=os.getpid())
    # Background task that logs the PID every few seconds.
    hb = asyncio.create_task(heartbeat(5.0), name="heartbeat")
    batch = 0
    try:
        while True:
            await run_batch(batch)
            batch += 1
            # Brief pause between batches so output stays readable.
            await asyncio.sleep(random.uniform(0.2, 1.0))
    except asyncio.CancelledError:
        log.info("cancelled", batches=batch)
    finally:
        hb.cancel()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
