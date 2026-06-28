"""Gevent stress app: saturate greenlane's event pipeline.

Spawns a large pool of tiny, hot greenlets that do almost nothing but yield to
the hub, driving the greenlet-switch rate as high as the machine allows. Each
``gevent.sleep(0)`` is two switches (greenlet -> hub -> greenlet), and each is
one event greenlane's trace hook must capture, serialise and stream — so this is
the worst case for the collector, the socket and the web timeline.

Run standalone (it self-reports the achievable switch rate):

    python tests/gevent_app_stress.py                 # 10000 spinners
    python tests/gevent_app_stress.py 50000           # more greenlets
    python tests/gevent_app_stress.py 10000 churn      # short-lived, respawned

Then attach greenlane to the printed PID to watch it melt.
"""

# Monkey-patch the stdlib BEFORE importing anything that does I/O.
from gevent import monkey

monkey.patch_all()

import logging
import os
import sys
import time

import gevent
import greenlet
import structlog
from gevent.pool import Pool

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


# ── Self-measurement ────────────────────────────────────────────────────────
# Count switches with the *same* mechanism greenlane uses (greenlet.settrace),
# so the reported rate reflects the real event volume — and so it keeps counting
# even after greenlane attaches (greenlane chains to this pre-existing hook).
_switches = [0]


def _count(event, args):
    _switches[0] += 1


def spinner(_i):
    """A maximally-cheap greenlet: yield to the hub forever, doing nothing."""
    while True:
        gevent.sleep(0)  # cooperative yield -> two switches


def burst(_i, rounds=4):
    """A short-lived greenlet: a few yields, then die (stresses greenlet
    creation/teardown and grows the distinct-ident count over time)."""
    for _ in range(rounds):
        gevent.sleep(0)


def report(n, mode):
    """Background greenlet: print switches/sec once a second."""
    last_n, last_t = _switches[0], time.perf_counter()
    log.info("stress started", greenlets=n, mode=mode, pid=os.getpid())
    while True:
        gevent.sleep(1.0)
        now_n, now_t = _switches[0], time.perf_counter()
        rate = (now_n - last_n) / (now_t - last_t)
        log.info("switches", per_sec=round(rate), total=now_n)
        last_n, last_t = now_n, now_t


def run_spin(n):
    """N long-lived spinners: maximum *sustained* switch rate, exactly N lanes."""
    greenlets = [gevent.spawn(spinner, i) for i in range(n)]
    gevent.joinall(greenlets)  # never returns; spinners loop forever


def run_churn(n):
    """Keep ~N greenlets alive, each short-lived and respawned: maximises both
    switch rate and the number of distinct greenlets greenlane has to track."""
    pool = Pool(n)
    i = 0
    while True:
        pool.wait_available()  # block until a slot frees
        pool.spawn(burst, i)
        i += 1


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 10_000
    mode = sys.argv[2] if len(sys.argv) > 2 else "spin"

    greenlet.settrace(_count)
    gevent.spawn(report, n, mode)

    try:
        if mode == "churn":
            run_churn(n)
        else:
            run_spin(n)
    except KeyboardInterrupt:
        log.info("stopping", total=_switches[0])


if __name__ == "__main__":
    main()
