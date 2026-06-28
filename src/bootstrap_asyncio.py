# greenlane bootstrap (asyncio variant) — injected into a live asyncio process
# via sys.remote_exec (PEP 768, CPython 3.14+). It emits the same binary trace
# format as the gevent bootstrap (ADR 0001; encoder inlined from src/glr.py), so
# the Rust collector, the slice store and the web viewer need *no* asyncio-specific
# handling — it streams `switch`/`gc`/`meta` frames over a Unix STREAM socket.
#
# ── The model ─────────────────────────────────────────────────────────────────
# gevent gives us a single, clean hook (`greenlet.settrace`) that fires on every
# greenlet switch. asyncio has no equivalent. The analogues are:
#
#     greenlet            ->  asyncio.Task           (one timeline lane each)
#     the Hub greenlet    ->  the event loop         (a synthetic "Loop" lane)
#     a greenlet switch   ->  a task *step*          (coro resumed -> next await)
#
# A task "step" is one turn of a coroutine on the loop: the loop resumes the
# task's coroutine, it runs until it awaits something that suspends, then control
# returns to the loop. That run-interval is exactly a greenlane slice. We emit:
#
#     • switch(Loop -> Task)  when a task step begins  (opens the task's slice)
#     • switch(Task -> Loop)  when that step ends       (opens the Loop's slice)
#
# so each task slice is precisely its step duration and the gaps between steps
# are attributed to "Loop" — mirroring how the gevent Hub dominates while idle.
#
# ── Why sys.monitoring (and not Task.__step) ───────────────────────────────────
# The obvious hook is to monkey-patch `asyncio.tasks.Task.__step`. But in stock
# CPython `asyncio.Task is _asyncio.Task` — the C-accelerated task, which has no
# Python `__step` to patch (same blind spot as uvloop's C tasks). So patching
# only works if a pure-Python task factory is installed. `sys.monitoring`
# (PEP 669) instruments coroutine *resume/yield* at the interpreter level and
# therefore works regardless of the task implementation — including uvloop.
#
# We watch PY_RESUME/PY_THROW (a coroutine resuming) and PY_YIELD/PY_RETURN/
# PY_UNWIND (a coroutine suspending or finishing), and use `current_task()` plus
# the "top-level frame" test to detect step boundaries.
#
# ── Stack capture ──────────────────────────────────────────────────────────────
# A coroutine resumes exactly where it suspended, so at the moment we resume a
# task its full await-chain is reachable from the task's coroutine via the
# `cr_await` links. We walk that chain (top -> leaf), reverse it to leaf -> root
# to match the gevent bootstrap, and attach it to the opening slice.
#
# We deliberately use `_socket` (the C module) rather than `socket`: the blocking
# raw socket can't accidentally re-enter the event loop from inside a monitoring
# callback the way a high-level / patched socket might.

import _socket
import gc
import sys
import threading
import time

try:
    import asyncio
except Exception:
    asyncio = None

_GH_SOCK_PATH = "__SOCKET_PATH__"
# Full call-stack capture mode (greenlane --include-traces): 0 off, 1 slow, 2 all.
# Walking the await chain is the expensive hot-path step, so it runs when a task
# STEP ends (its duration known) on the task that just suspended: `all` walks every
# step, `slow` only steps at/over the warn threshold, `off` never. Every step still
# gets a cheap leaf-function label. sys.monitoring fires on the hot path, so this
# gating matters as much as it does for the gevent hook.
_GH_MODE = __TRACE_MODE__
# Warn threshold (ns) — the slow/fast cutoff for `slow` mode. Matches --warn-ms.
_GH_WARN_NS = __WARN_NS__

# Synthetic identity for the event loop. Real id() values are addresses and are
# never this small, so 1 can't collide with a real Task.
_LOOP_ID = 1


# ── binary trace encoder (inlined from src/glr.py) ──────────────────────────
# __GLR_ENCODER__
# ────────────────────────────────────────────────────────────────────────────


def _pyinfo_json(runtime):
    """Interpreter/runtime facts for the viewer's System panel (best-effort)."""
    import json
    import os
    import platform

    info = {
        "runtime": runtime,
        "python": platform.python_version(),
        "implementation": platform.python_implementation(),
        "executable": sys.executable,
        "pid": os.getpid(),
        "thread": threading.current_thread().name,
        "platform": sys.platform,
    }
    try:
        return json.dumps(info)
    except Exception:
        return "{}"


def _greenlane_install():
    if asyncio is None:
        return

    mon = sys.monitoring
    E = mon.events
    # PROFILER_ID is the conventional slot for a sampling/timeline tool. If
    # something already owns it, fall back to any free id.
    tool_id = mon.PROFILER_ID
    try:
        mon.use_tool_id(tool_id, "greenlane")
    except ValueError:
        tool_id = None
        for cand in range(6):
            try:
                mon.use_tool_id(cand, "greenlane")
                tool_id = cand
                break
            except ValueError:
                continue
        if tool_id is None:
            return  # no monitoring slot available; nothing we can do

    sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    sock.connect(_GH_SOCK_PATH)
    # Bound how long a send may block the target: if greenlane stalls or goes
    # away, the next flush raises instead of hanging the profiled event loop.
    sock.settimeout(5.0)

    t0 = time.perf_counter_ns()
    import os

    # Send buffer + binary encoder. Task steps are extremely hot, so instead of one
    # sendall per step we append encoded frames here and flush in batches once the
    # buffer crosses _FLUSH_BYTES (or _FLUSH_NS of wall time passes, so a low-rate
    # loop still delivers promptly). sys.monitoring is interpreter-global and fires
    # on every thread, so a lock guards BOTH the buffer and the encoder's pools when
    # several event loops run on different threads; it's uncontended (and cheap) in
    # the common single-loop case.
    _buf = bytearray()
    enc = GlrEnc(_buf)  # writes the stream header into _buf
    _FLUSH_BYTES = 16384
    _FLUSH_NS = 50_000_000  # 50ms
    _last_flush = [0]
    _lock = threading.Lock()

    def _flush(now_ns):
        # Returns False if greenlane went away (caller tears the hook down).
        # Must be called WITHOUT _lock held (it takes it to snapshot).
        _last_flush[0] = now_ns
        # Snapshot + clear under the lock BEFORE the blocking send: sendall releases
        # the GIL, and clearing only what we snapshotted means a concurrent append
        # from another loop thread is preserved for the next flush, not dropped.
        with _lock:
            if not _buf:
                return True
            chunk = bytes(_buf)
            del _buf[:]
        try:
            sock.sendall(chunk)
        except OSError:
            return False
        return True

    # Schemas (switch + gc) and the meta frame (epoch wall-clock, loop tid, pid,
    # interpreter facts) — one-time, sent before any events on this install thread.
    enc.write_wire_schemas()
    enc.meta(
        int(time.time() * 1000), threading.get_native_id(), os.getpid(), _pyinfo_json("asyncio")
    )
    _flush(0)

    _LIB = ("/asyncio/", "/greenlane")  # library frames (kept, but flagged)

    # Per-thread state. sys.monitoring is interpreter-global (fires on every
    # thread), but each event loop runs on its own thread with its own
    # current_task(), so the "currently running unit" and the OS thread id must be
    # tracked per thread — a single shared value would let one loop's step close
    # another loop's span. The collector keys its run-intervals by the emitted
    # thread id for the same reason.
    _tls = threading.local()

    def _running_get():
        try:
            return _tls.running
        except AttributeError:
            _tls.running = _LOOP_ID
            return _LOOP_ID

    def _running_set(v):
        _tls.running = v

    def _tid():
        try:
            return _tls.tid
        except AttributeError:
            _tls.tid = threading.get_native_id()
            return _tls.tid

    def _in_cb_get():
        return getattr(_tls, "in_cb", False)

    def _in_cb_set(v):
        _tls.in_cb = v

    def _running_task_get():
        # The Task object currently running on this thread (None when on the Loop) —
        # kept so we can walk its yield point when its step CLOSES.
        return getattr(_tls, "running_task", None)

    def _running_task_set(v):
        _tls.running_task = v

    def _step_start_get():
        return getattr(_tls, "step_start", 0)

    def _step_start_set(v):
        _tls.step_start = v

    def _coro_frame(obj):
        # The frame of a coroutine / async-gen / generator, if it has one.
        return (
            getattr(obj, "cr_frame", None)
            or getattr(obj, "ag_frame", None)
            or getattr(obj, "gi_frame", None)
        )

    def _coro_await(obj):
        # The next awaitable down the chain (what `obj` is currently awaiting).
        return (
            getattr(obj, "cr_await", None)
            or getattr(obj, "ag_await", None)
            or getattr(obj, "gi_yieldfrom", None)
        )

    def _task_frames(task, limit=32):
        # Walk the suspended task's await chain (top coroutine -> what it awaits
        # -> ... -> leaf), collecting one entry per coroutine frame. This is the
        # resume point: where each coroutine will continue when stepped. We then
        # reverse to leaf -> root so it matches the gevent bootstrap's ordering.
        # Each entry is "fullpath:qualname:lineno".
        out = []
        try:
            obj = task.get_coro()
        except Exception:
            return out
        seen = 0
        while obj is not None and seen < limit:
            fr = _coro_frame(obj)
            if fr is not None:
                co = fr.f_code
                fn = co.co_filename
                if "greenlane-bootstrap" not in fn:
                    out.append("%s:%s:%d" % (fn, co.co_qualname, fr.f_lineno))
            obj = _coro_await(obj)
            seen += 1
        out.reverse()  # leaf -> root, same as walking f_back in the gevent hook
        return out

    def _task_func_cheap(task, limit=32):
        # Cheap leaf label when full traces are off: walk the await chain but only
        # remember the deepest application frame (where the coroutine actually is),
        # formatted as one "basename:qualname:lineno". Avoids building + reversing +
        # joining the whole chain on every task step.
        try:
            obj = task.get_coro()
        except Exception:
            return ""
        leaf_app = ""
        leaf_any = ""
        seen = 0
        while obj is not None and seen < limit:
            fr = _coro_frame(obj)
            if fr is not None:
                co = fr.f_code
                fn = co.co_filename
                if "greenlane-bootstrap" not in fn:
                    ent = "%s:%s:%d" % (fn.rpartition("/")[2], co.co_qualname, fr.f_lineno)
                    leaf_any = ent
                    if not any(s in fn for s in _LIB):
                        leaf_app = ent
            obj = _coro_await(obj)
            seen += 1
        return leaf_app or leaf_any

    def _task_label(task):
        # A whitespace-free identity. Tasks carry a name (auto "Task-N" or the
        # app-supplied name passed to create_task(..., name=...)); we sanitise
        # whitespace to keep labels compact and consistent with the gevent side.
        try:
            name = task.get_name()
        except Exception:
            name = "Task"
        return str(name).replace("\t", " ").replace("\n", " ").replace(" ", "-")

    def _task_corr(task):
        # An app-set correlation id stashed on the task object, if any. (Tasks
        # are objects, so apps can do `t.request_id = ...` just like greenlets.)
        for attr in ("request_id", "task_id", "trace_id"):
            v = getattr(task, attr, None)
            if v is not None:
                return str(v).replace("\t", " ").replace("\n", " ")
        return ""

    def _emit(now, target_id, label, func, corr, closing_task):
        # Encode one switch: `func`/`label` describe the OPENING span (target); the
        # full `stack` describes the CLOSING span — the task that just suspended
        # (`closing_task`, None for the Loop). The collector attaches func to the
        # opener and stack to the closer. Walking the closing task's await chain
        # happens only when the mode (and, for `slow`, its step duration) calls for
        # it — that's what keeps `slow` cheap. The build runs under _lock so the
        # encoder pools + buffer stay consistent across loop threads.
        stack_frames = ()
        if closing_task is not None:
            if _GH_MODE == 2:  # all
                stack_frames = _task_frames(closing_task)
            elif _GH_MODE == 1:  # slow
                if now - _step_start_get() >= _GH_WARN_NS:
                    stack_frames = _task_frames(closing_task)
        with _lock:
            enc.switch(
                now,
                target_id,
                enc.str_id(label),
                enc.str_id(func),
                enc.str_id(corr),
                enc.stack_id(stack_frames),
                _tid(),
            )
            over = len(_buf) >= _FLUSH_BYTES or now - _last_flush[0] >= _FLUSH_NS
        if over and not _flush(now):
            _teardown()

    def _switch_to_task(task):
        # A task step is beginning: close the current span (the Loop, normally),
        # open the task's. The task's full stack is captured later, when its step
        # ends (_switch_to_loop). Here we record only its cheap resume leaf.
        tid = id(task)
        if tid == _running_get():
            return
        now = time.perf_counter_ns() - t0
        _emit(
            now,
            tid,
            _task_label(task),
            _task_func_cheap(task),
            _task_corr(task),
            _running_task_get(),
        )
        _running_set(tid)
        _running_task_set(task)
        _step_start_set(now)

    def _switch_to_loop():
        # The running task step just ended: close the task's slice (walking its
        # yield point if it was slow / mode=all), open the Loop's.
        if _running_get() == _LOOP_ID:
            return
        now = time.perf_counter_ns() - t0
        _emit(now, _LOOP_ID, "Loop", "", "", _running_task_get())
        _running_set(_LOOP_ID)
        _running_task_set(None)
        _step_start_set(now)

    def _current_task():
        try:
            return asyncio.current_task()
        except RuntimeError:
            return None  # no running loop on this thread

    # CO_COROUTINE flag on a code object (its frames are coroutine steps).
    _CO_COROUTINE = 0x0080

    # ── monitoring callbacks ──────────────────────────────────────────────────
    # PY_RESUME/PY_THROW: a coroutine is resuming. If current_task() differs from
    # who we think is running, a new task step has begun.
    def _on_resume(code, offset):
        if _in_cb_get():
            return
        task = _current_task()
        if task is not None and id(task) != _running_get():
            _in_cb_set(True)
            try:
                _switch_to_task(task)
            finally:
                _in_cb_set(False)

    # PY_START fires when a code object begins — including a task coroutine's FIRST
    # step (which is a start, not a resume), so without this a task that runs a long
    # first step before its first await — e.g. a CPU-bound task — would be
    # mis-attributed to the Loop. PY_START also fires for ordinary function calls,
    # so we return monitoring.DISABLE for non-coroutine code: the interpreter then
    # stops calling us for that code object, leaving only coroutine starts after a
    # brief warmup (cheap). Coroutine starts route to the same task-step logic.
    def _on_start(code, offset):
        if not (code.co_flags & _CO_COROUTINE):
            return mon.DISABLE
        _on_resume(code, offset)

    def _on_throw(code, offset, exc):
        if _in_cb_get():
            return
        task = _current_task()
        if task is not None and id(task) != _running_get():
            _in_cb_set(True)
            try:
                _switch_to_task(task)
            finally:
                _in_cb_set(False)

    # PY_YIELD/PY_RETURN/PY_UNWIND on a *top-level* coroutine frame marks the end
    # of a step: control is about to return to the loop. A task's top coroutine
    # is the one driven directly by the loop, so its frame's parent is either the
    # loop's `Handle._run` (stock asyncio drives steps through a callback handle)
    # or None (uvloop's C driver, or an eagerly-started task). Nested awaited
    # coroutines have an ordinary Python parent and are ignored.
    def _on_suspend(code, offset, *rest):
        if _in_cb_get() or _running_get() == _LOOP_ID:
            return
        parent = sys._getframe(1).f_back  # caller of the coroutine raising this
        if parent is None or parent.f_code.co_qualname == "Handle._run":
            _in_cb_set(True)
            try:
                _switch_to_loop()
            finally:
                _in_cb_set(False)

    mon.register_callback(tool_id, E.PY_START, _on_start)
    mon.register_callback(tool_id, E.PY_RESUME, _on_resume)
    mon.register_callback(tool_id, E.PY_THROW, _on_throw)
    mon.register_callback(tool_id, E.PY_YIELD, _on_suspend)
    mon.register_callback(tool_id, E.PY_RETURN, _on_suspend)
    mon.register_callback(tool_id, E.PY_UNWIND, _on_suspend)
    mon.set_events(
        tool_id,
        E.PY_START | E.PY_RESUME | E.PY_THROW | E.PY_YIELD | E.PY_RETURN | E.PY_UNWIND,
    )

    # ── GC tracking ─────────────────────────────────────────────────────────
    # Identical to the gevent bootstrap: a GC pause blocks the whole loop thread,
    # so timing each collection explains timeline-wide stalls.
    _gc_start = [0]

    def _gc_cb(phase, info):
        if phase == "start":
            _gc_start[0] = time.perf_counter_ns()
        elif phase == "stop":
            if _gc_start[0] == 0:
                return  # "stop" without a matching "start" (installed mid-collection)
            now = time.perf_counter_ns()
            start = _gc_start[0] - t0
            if start < 0:
                start = 0
            with _lock:
                enc.gc(
                    start, now - _gc_start[0], info.get("generation", -1), info.get("collected", 0)
                )
            # GC marks a stall worth delivering promptly, so flush right after.
            if not _flush(now - t0):
                _teardown()  # greenlane gone: stop tracking

    gc.callbacks.append(_gc_cb)

    def _teardown():
        # greenlane went away (exit / detach): stop paying the monitoring cost
        # and detach cleanly so the target is left exactly as we found it.
        # Best-effort final flush first so a clean detach doesn't drop buffered
        # events (a no-op / harmless failure when the socket is already gone).
        try:
            _flush(time.perf_counter_ns() - t0)
        except Exception:
            pass
        try:
            mon.set_events(tool_id, mon.events.NO_EVENTS)
            for ev in (E.PY_START, E.PY_RESUME, E.PY_THROW, E.PY_YIELD, E.PY_RETURN, E.PY_UNWIND):
                mon.register_callback(tool_id, ev, None)
            mon.free_tool_id(tool_id)
        except Exception:
            pass
        try:
            gc.callbacks.remove(_gc_cb)
        except ValueError:
            pass
        try:
            sock.close()
        except OSError:
            pass

    # Keep references alive past this function's scope so they aren't GC'd.
    import builtins

    builtins.__greenlane_sock = sock
    builtins.__greenlane_cbs = (_on_resume, _on_throw, _on_suspend)
    builtins.__greenlane_gc = _gc_cb
    builtins.__greenlane_teardown = _teardown


_greenlane_install()
