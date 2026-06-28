# greenlane bootstrap — injected into a live gevent process via sys.remote_exec
# (PEP 768, CPython 3.14+). Registers a greenlet trace hook and streams switch
# events to greenlane over a Unix STREAM socket, encoded with the binary trace
# format (see ADR 0001). The encoder below the marker is inlined from src/glr.py
# at materialization and mirrors the Rust decoder (src/trace_format.rs).
#
# We deliberately use `_socket` (the C module) rather than `socket`, because a
# gevent-monkey-patched process replaces `socket.socket` with a cooperative
# greenlet-aware socket. Using the patched socket from inside the trace hook
# would re-enter the hub and recurse. `_socket` is the raw, unpatched impl.

import _socket
import gc
import sys
import threading
import time

try:
    import greenlet
except Exception:
    greenlet = None

_GH_SOCK_PATH = "__SOCKET_PATH__"
# Full call-stack capture mode (greenlane --include-traces): 0 off, 1 slow, 2 all.
# The stack walk is the expensive hot-path step, so it runs at a execution's *close*
# (when its duration is known) on the greenlet that just yielded: `all` walks every
# execution, `slow` only executions at/over the warn threshold, `off` never. Every execution still
# gets a cheap leaf-function label regardless.
_GH_MODE = __TRACE_MODE__
# Warn threshold (ns) — the slow/fast cutoff for `slow` mode. Matches --warn-ms.
_GH_WARN_NS = __WARN_NS__


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
        import gevent

        info["gevent"] = getattr(gevent, "__version__", "?")
    except Exception:
        pass
    try:
        return json.dumps(info)
    except Exception:
        return "{}"


def _greenlane_install():
    if greenlet is None:
        return

    sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    sock.connect(_GH_SOCK_PATH)
    # Bound how long a send may block the target's hub thread: if greenlane stalls
    # or goes away, the next flush raises instead of hanging the profiled app.
    sock.settimeout(5.0)

    # Bind hot built-ins to locals: `_perf` (read once per switch) and `_getframe`
    # (the leaf-label walk) are on the per-switch hot path, so skip the attribute
    # lookups there.
    _perf = time.perf_counter_ns
    _getframe = sys._getframe

    t0 = _perf()
    prev = greenlet.gettrace()

    # greenlet.settrace is per-thread, so every switch this hook sees runs on the
    # install thread. Stamp each event with its OS thread id so the collector keeps
    # a separate run-interval per thread (an app may drive several gevent hubs on
    # different threads, each streaming over this one socket).
    _tid = threading.get_native_id()
    import os

    # Send buffer + binary encoder. Switches are extremely hot, so instead of one
    # sendall per switch we append encoded frames here and flush in batches once
    # the buffer crosses _FLUSH_BYTES — cutting syscalls (and target overhead) at
    # high switch rates. The hub runs between greenlets, so the buffer drains
    # continuously; a quiet app simply has little to flush.
    _buf = bytearray()
    enc = GlrEnc(_buf)  # writes the stream header into _buf
    _FLUSH_BYTES = 16384
    # Also flush after this much wall time even if the buffer is small, so a
    # low-rate target doesn't sit on a partial buffer for ages (which would make
    # the viewer's data lag grow without bound). `_last_flush[0]` is ns since t0.
    _FLUSH_NS = 50_000_000  # 50ms
    _last_flush = [0]

    def _flush(now_ns):
        # Returns False if greenlane went away (caller tears the hook down).
        _last_flush[0] = now_ns
        if not _buf:
            return True
        try:
            # Send the bytearray directly (buffer protocol) — no `bytes()` copy of
            # up to _FLUSH_BYTES per flush. Safe under the single-hub-thread model
            # (see the GC note below): nothing appends to _buf during the send.
            sock.sendall(_buf)
        except OSError:
            return False
        del _buf[:]
        return True

    # Schemas (switch + gc) and the meta frame (epoch wall-clock, hub tid, pid,
    # interpreter facts) — all one-time, sent before any events.
    enc.write_wire_schemas()
    enc.meta(int(time.time() * 1000), _tid, os.getpid(), _pyinfo_json("gevent"))
    _flush(0)

    def _headline_cheap(limit=32):
        # Cheap leaf label for the *resuming* greenlet: this hook runs in the
        # target's context, so walking up from here finds where it's about to run.
        # Walk only to the first application frame, format that one
        # ("basename:qualname:lineno"), and stop. This is the per-execution `func` and is
        # captured on EVERY switch regardless of trace mode — so the library-frame
        # test is inlined (no generator/tuple) to keep this walk cheap at high rates.
        f = _getframe()
        n = 0
        while f is not None and n < limit:
            fn = f.f_code.co_filename
            if (
                fn != "<string>"
                and "greenlane-bootstrap" not in fn
                and "/gevent/" not in fn
                and "/greenlet" not in fn
            ):
                co = f.f_code
                return "%s:%s:%d" % (fn.rpartition("/")[2], co.co_qualname, f.f_lineno)
            f = f.f_back
            n += 1
        return ""

    def _walk_greenlet(g, limit=32):
        # Full call chain (leaf → root) of a SUSPENDED greenlet, from its `gr_frame`
        # (where it yielded). Used at a execution's close to capture the slow greenlet's
        # yield point. Keeps every frame (incl. gevent/stdlib); the client trims an
        # app-only view for the hover. Each entry is "fullpath:qualname:lineno".
        out = []
        f = getattr(g, "gr_frame", None)
        while f is not None and len(out) < limit:
            fn = f.f_code.co_filename
            if fn != "<string>" and "greenlane-bootstrap" not in fn:
                co = f.f_code
                out.append("%s:%s:%d" % (fn, co.co_qualname, f.f_lineno))
            f = f.f_back
        return out

    def _is_hub(g):
        return type(g).__name__[:3].lower() == "hub"

    def _task_id(g):
        # An app-set correlation id on the greenlet, if any. Read straight from the
        # instance __dict__ (one fetch + cheap dict.gets) instead of three getattr
        # calls per switch — the common case (no correlation id) then costs almost
        # nothing. Correlation ids are conventionally set as instance attributes, so
        # this sees them; an exotic descriptor-based id would be missed.
        d = getattr(g, "__dict__", None)
        if d:
            for attr in ("request_id", "task_id", "trace_id"):
                v = d.get(attr)
                if v is not None:
                    return str(v).replace("\n", " ")
        return ""

    # Timestamp (ns since t0) at which the currently-running greenlet was switched
    # in — so at the next switch we know how long it ran (whether it's "slow").
    _open_ts = [0]

    def _teardown():
        # greenlane went away (exit / detach): remove ourselves so the target
        # stops paying the trace cost, and restore any prior hook. Best-effort
        # final flush first so a clean detach doesn't drop buffered events.
        try:
            _flush(time.perf_counter_ns() - t0)
        except Exception:
            pass
        try:
            sock.close()
        except OSError:
            pass
        greenlet.settrace(prev)

    def _cb(event, args):
        # event is "switch" or "throw" (a throw is also a context switch); args is
        # (origin, target): `origin` just yielded (its execution CLOSES now), `target` is
        # resuming (its execution OPENS now). Each event carries the opening execution's label
        # + cheap leaf `func`, and the CLOSING execution's full `stack` (gated by mode):
        # the collector attaches func to the opening execution and stack to the closing
        # one. Walking the closing greenlet's frame at close — only when needed —
        # is what makes `slow` cheap.
        if event == "switch" or event == "throw":
            origin, target = args
            # A whitespace-free identity: type name + gevent's minimal_ident,
            # e.g. "Hub", "Greenlet-3". The type prefix disambiguates the hub
            # (which dominates running time while blocked in the event loop)
            # from worker greenlets that may share a minimal_ident value.
            name = type(target).__name__
            mid = getattr(target, "minimal_ident", None)
            label = name if mid is None else "%s-%s" % (name, mid)
            ts = _perf() - t0
            # Stack for the CLOSING greenlet (origin). Walk its yield point only
            # when the mode asks and (for `slow`) the execution was actually slow and not
            # the Hub — so the expensive walk happens for the executions worth it.
            stack_frames = ()
            if _GH_MODE == 2:  # all
                stack_frames = _walk_greenlet(origin)
            elif _GH_MODE == 1:  # slow
                dur = ts - _open_ts[0]
                if dur >= _GH_WARN_NS and not _is_hub(origin):
                    stack_frames = _walk_greenlet(origin)
            _open_ts[0] = ts
            # Intern strings/stack (emits pool frames on first sight), then the
            # switch event referencing their ids + the OS thread id.
            enc.switch(
                ts,
                id(target),
                enc.str_id(label),
                enc.str_id(_headline_cheap()),  # cheap resume leaf for the opener
                enc.str_id(_task_id(target)),
                enc.stack_id(stack_frames),  # yield-point stack of the closer
                _tid,
            )
            if (len(_buf) >= _FLUSH_BYTES or ts - _last_flush[0] >= _FLUSH_NS) and not _flush(ts):
                _teardown()
                return prev(event, args) if prev is not None else None
        # be polite: chain to any pre-existing tracer
        if prev is not None:
            try:
                return prev(event, args)
            except Exception:
                pass

    greenlet.settrace(_cb)

    # ── GC tracking ─────────────────────────────────────────────────────────
    # A GC pause blocks the whole gevent thread (every greenlet), so timing each
    # collection explains timeline-wide stalls. gc.callbacks fires start/stop.
    # (Assumes the single-hub-thread model: the encoder is shared with _cb without
    # a lock, as the trace hook and GC run on the same thread under the GIL.)
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
            enc.gc(start, now - _gc_start[0], info.get("generation", -1), info.get("collected", 0))
            # GC is rare and marks a stall worth delivering promptly.
            if not _flush(now - t0):
                try:
                    gc.callbacks.remove(_gc_cb)  # greenlane gone: stop tracking
                except ValueError:
                    pass

    gc.callbacks.append(_gc_cb)

    # Keep references alive past this function's scope so they aren't GC'd.
    import builtins

    builtins.__greenlane_sock = sock
    builtins.__greenlane_cb = _cb
    builtins.__greenlane_gc = _gc_cb
    builtins.__greenlane_teardown = _teardown


_greenlane_install()
