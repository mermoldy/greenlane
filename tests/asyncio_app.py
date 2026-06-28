"""Asyncio demo app: run 10 tasks with a mix of I/O-bound and CPU-bound
workloads, each doing random cooperative blocking. The asyncio analogue of
``gevent_app.py`` — used to exercise greenlane's asyncio bootstrap.

Run with:  python tests/asyncio_app.py
"""

import asyncio
import os
import random
import time


def log(msg):
    """Timestamped log line including the current task's name."""
    try:
        task = asyncio.current_task()
        name = task.get_name() if task is not None else "?"
    except RuntimeError:
        name = "?"
    print(f"[{time.strftime('%H:%M:%S')}] [{name}] {msg}", flush=True)


async def io_bound(worker_id):
    """Simulate I/O work: cooperative sleeps that yield to other tasks."""
    rounds = random.randint(3, 6)
    log(f"worker {worker_id} (IO) starting, {rounds} rounds")
    for r in range(rounds):
        # asyncio.sleep yields to the event loop so other tasks can run.
        delay = random.uniform(0.1, 0.8)
        log(f"worker {worker_id} (IO) round {r}: awaiting {delay:.2f}s")
        await asyncio.sleep(delay)
    log(f"worker {worker_id} (IO) done")
    return ("io", worker_id, rounds)


async def cpu_bound(worker_id):
    """Simulate CPU work: a tight loop that occasionally yields so the event
    loop is not starved. Real CPU work blocks the loop, so we ``await
    asyncio.sleep(0)`` periodically to keep things cooperative."""
    iterations = random.randint(2, 5)
    log(f"worker {worker_id} (CPU) starting, {iterations} chunks")
    total = 0
    for c in range(iterations):
        n = random.randint(200_000, 600_000)
        log(f"worker {worker_id} (CPU) chunk {c}: crunching {n} ints")
        for i in range(n):
            total += i * i
        # Yield to the loop, with a random extra blocking sleep mixed in.
        await asyncio.sleep(random.uniform(0.0, 0.3))
    log(f"worker {worker_id} (CPU) done, total={total}")
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
        log(f"[heartbeat] alive -- PID={os.getpid()}")
        await asyncio.sleep(interval)


async def run_batch(batch):
    """Spawn 10 tasks, run them to completion, report a summary."""
    log(f"=== batch {batch}: spawning 10 tasks ===")
    start = time.time()

    tasks = [asyncio.create_task(worker(i), name=f"worker-{i}") for i in range(10)]
    results = await asyncio.gather(*tasks)

    elapsed = time.time() - start
    io_count = sum(1 for r in results if r and r[0] == "io")
    cpu_count = sum(1 for r in results if r and r[0] == "cpu")
    log(f"=== batch {batch} done in {elapsed:.2f}s  (io={io_count}, cpu={cpu_count}) ===")


async def main():
    random.seed(os.getpid())
    log(f"starting up -- PID={os.getpid()}")
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
        log(f"cancelled -- stopping after {batch} batches. Bye!")
    finally:
        hb.cancel()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
