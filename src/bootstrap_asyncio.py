# greenlane bootstrap (asyncio variant) — injected into a live asyncio process
# via sys.remote_exec (PEP 768, CPython 3.14+). It reproduces the gevent
# bootstrap's wire protocol exactly, so the Rust collector, the slice store and
# the web viewer need *no* changes: it streams the same tab-delimited
# "switch"/"gc"/"meta" lines over a Unix STREAM socket.
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
import time

try:
    import asyncio
except Exception:
    asyncio = None

_GH_SOCK_PATH = "__SOCKET_PATH__"

# Synthetic identity for the event loop. Real id() values are addresses and are
# never this small, so 1 can't collide with a real Task.
_LOOP_ID = 1


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

    t0 = time.perf_counter_ns()

    # Header: wall-clock epoch (ms) at t0, so the viewer can show absolute time.
    try:
        sock.sendall(b"meta\t%d\n" % int(time.time() * 1000))
    except OSError:
        pass

    _LIB = ("/asyncio/", "/greenlane")  # library frames (kept, but flagged)

    # The "unit" (task id) currently considered running. Starts on the Loop.
    _running = [_LOOP_ID]
    # Guard against a callback re-entering us (it shouldn't, but be safe).
    _in_cb = [False]

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

    def _headline(frames):
        # Compact label = first application frame (basename), skipping library.
        for fr in frames:
            path = fr.rsplit(":", 2)[0]
            if not any(s in path for s in _LIB):
                base, _, rest = fr.rpartition("/")
                return rest if base else fr
        if frames:
            _, _, rest = frames[0].rpartition("/")
            return rest or frames[0]
        return ""

    def _task_label(task):
        # A whitespace-free identity. Tasks carry a name (auto "Task-N" or the
        # app-supplied name passed to create_task(..., name=...)); we sanitise
        # whitespace so it survives the tab-delimited protocol.
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

    def _send_switch(event, origin_id, target_id, label, func, corr, stack):
        # Same field order/semantics as the gevent bootstrap. The collector
        # attaches label/func/task/stack to the *target* (opening) span.
        #   t_ns \t event \t origin \t target \t label \t func \t task \t stack
        line = "%d\t%s\t%d\t%d\t%s\t%s\t%s\t%s\n" % (
            time.perf_counter_ns() - t0,
            event,
            origin_id,
            target_id,
            label,
            func,
            corr,
            stack,
        )
        try:
            sock.sendall(line.encode())
            return True
        except OSError:
            _teardown()
            return False

    def _switch_to_task(task, event):
        # A task step is beginning: close the Loop's slice, open the task's.
        tid = id(task)
        if tid == _running[0]:
            return
        frames = _task_frames(task)
        label = _task_label(task)
        _send_switch(
            event,
            _running[0],
            tid,
            label,
            _headline(frames),
            _task_corr(task),
            " <- ".join(frames),
        )
        _running[0] = tid

    def _switch_to_loop():
        # The running task step just ended: close its slice, open the Loop's.
        if _running[0] == _LOOP_ID:
            return
        _send_switch("switch", _running[0], _LOOP_ID, "Loop", "", "", "")
        _running[0] = _LOOP_ID

    def _current_task():
        try:
            return asyncio.current_task()
        except RuntimeError:
            return None  # no running loop on this thread

    # ── monitoring callbacks ──────────────────────────────────────────────────
    # PY_RESUME/PY_THROW: a coroutine is resuming. If current_task() differs from
    # who we think is running, a new task step has begun.
    def _on_resume(code, offset):
        if _in_cb[0]:
            return
        task = _current_task()
        if task is not None and id(task) != _running[0]:
            _in_cb[0] = True
            try:
                _switch_to_task(task, "switch")
            finally:
                _in_cb[0] = False

    def _on_throw(code, offset, exc):
        if _in_cb[0]:
            return
        task = _current_task()
        if task is not None and id(task) != _running[0]:
            _in_cb[0] = True
            try:
                _switch_to_task(task, "throw")
            finally:
                _in_cb[0] = False

    # PY_YIELD/PY_RETURN/PY_UNWIND on a *top-level* coroutine frame marks the end
    # of a step: control is about to return to the loop. A task's top coroutine
    # is the one driven directly by the loop, so its frame's parent is either the
    # loop's `Handle._run` (stock asyncio drives steps through a callback handle)
    # or None (uvloop's C driver, or an eagerly-started task). Nested awaited
    # coroutines have an ordinary Python parent and are ignored.
    def _on_suspend(code, offset, *rest):
        if _in_cb[0] or _running[0] == _LOOP_ID:
            return
        parent = sys._getframe(1).f_back  # caller of the coroutine raising this
        if parent is None or parent.f_code.co_qualname == "Handle._run":
            _in_cb[0] = True
            try:
                _switch_to_loop()
            finally:
                _in_cb[0] = False

    mon.register_callback(tool_id, E.PY_RESUME, _on_resume)
    mon.register_callback(tool_id, E.PY_THROW, _on_throw)
    mon.register_callback(tool_id, E.PY_YIELD, _on_suspend)
    mon.register_callback(tool_id, E.PY_RETURN, _on_suspend)
    mon.register_callback(tool_id, E.PY_UNWIND, _on_suspend)
    mon.set_events(
        tool_id,
        E.PY_RESUME | E.PY_THROW | E.PY_YIELD | E.PY_RETURN | E.PY_UNWIND,
    )

    # ── GC tracking ─────────────────────────────────────────────────────────
    # Identical to the gevent bootstrap: a GC pause blocks the whole loop thread,
    # so timing each collection explains timeline-wide stalls.
    _gc_start = [0]

    def _gc_cb(phase, info):
        if phase == "start":
            _gc_start[0] = time.perf_counter_ns()
        elif phase == "stop":
            now = time.perf_counter_ns()
            line = "gc\t%d\t%d\t%d\t%d\n" % (
                _gc_start[0] - t0,
                now - _gc_start[0],
                info.get("generation", -1),
                info.get("collected", 0),
            )
            try:
                sock.sendall(line.encode())
            except OSError:
                try:
                    gc.callbacks.remove(_gc_cb)
                except ValueError:
                    pass

    gc.callbacks.append(_gc_cb)

    def _teardown():
        # greenlane went away (exit / detach): stop paying the monitoring cost
        # and detach cleanly so the target is left exactly as we found it.
        try:
            mon.set_events(tool_id, mon.events.NO_EVENTS)
            for ev in (E.PY_RESUME, E.PY_THROW, E.PY_YIELD, E.PY_RETURN, E.PY_UNWIND):
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
