# greenlane bootstrap — injected into a live gevent process via sys.remote_exec
# (PEP 768, CPython 3.14+). Registers a greenlet trace hook and streams switch
# events to greenlane over a Unix STREAM socket.
#
# We deliberately use `_socket` (the C module) rather than `socket`, because a
# gevent-monkey-patched process replaces `socket.socket` with a cooperative
# greenlet-aware socket. Using the patched socket from inside the trace hook
# would re-enter the hub and recurse. `_socket` is the raw, unpatched impl.

import _socket
import gc
import sys
import time

try:
    import greenlet
except Exception:
    greenlet = None

_GH_SOCK_PATH = "__SOCKET_PATH__"


def _greenlane_install():
    if greenlet is None:
        return

    sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    sock.connect(_GH_SOCK_PATH)

    t0 = time.perf_counter_ns()
    prev = greenlet.gettrace()

    # Header: wall-clock epoch (ms) at t0, so the viewer can show absolute time.
    try:
        sock.sendall(b"meta\t%d\n" % int(time.time() * 1000))
    except OSError:
        pass

    _LIB = ("/gevent/", "/greenlet")  # library frames (kept, but flagged for UI)

    def _frames(limit=32):
        # greenlet runs this trace hook in the *target's* context (after the
        # switch), so walking up from here collects the resuming greenlet's full
        # call chain (leaf → root). We keep *everything* except the bootstrap
        # itself — full file paths and all frames (incl. gevent/stdlib) — so the
        # detail panel can be exhaustive; the client trims a short app-only view
        # for the hover. Each entry is "fullpath:qualname:lineno".
        out = []
        f = sys._getframe()
        while f is not None and len(out) < limit:
            fn = f.f_code.co_filename
            if fn != "<string>" and "greenlane-bootstrap" not in fn:
                co = f.f_code
                out.append("%s:%s:%d" % (fn, co.co_qualname, f.f_lineno))
            f = f.f_back
        return out

    def _headline(frames):
        # Compact label = first application frame (basename), skipping library.
        for fr in frames:
            path = fr.rsplit(":", 2)[0]
            if not any(s in path for s in _LIB):
                base, _, rest = fr.rpartition("/")  # drop dir
                return rest if base else fr
        if frames:
            _, _, rest = frames[0].rpartition("/")
            return rest or frames[0]
        return ""

    def _task_id(g):
        # An app-set correlation id on the greenlet, if any.
        for attr in ("request_id", "task_id", "trace_id"):
            v = getattr(g, attr, None)
            if v is not None:
                return str(v).replace("\t", " ").replace("\n", " ")
        return ""

    def _cb(event, args):
        # event is "switch" or "throw"; args is (origin, target). func/task
        # describe `target` — the greenlet resuming now (whose run-interval
        # begins here); the collector attaches them to that opening span.
        if event == "switch" or event == "throw":
            origin, target = args
            # A whitespace-free identity: type name + gevent's minimal_ident,
            # e.g. "Hub", "Greenlet-3". The type prefix disambiguates the hub
            # (which dominates running time while blocked in the event loop)
            # from worker greenlets that may share a minimal_ident value.
            name = type(target).__name__
            mid = getattr(target, "minimal_ident", None)
            label = name if mid is None else "%s-%s" % (name, mid)
            frames = _frames()
            func = _headline(frames)        # compact app-frame label
            stack = " <- ".join(frames)     # full chain, full paths, all frames
            # Tab-delimited so fields may contain spaces:
            #   t_ns \t event \t origin \t target \t label \t func \t task \t stack
            line = "%d\t%s\t%d\t%d\t%s\t%s\t%s\t%s\n" % (
                time.perf_counter_ns() - t0,
                event,
                id(origin),
                id(target),
                label,
                func,
                _task_id(target),
                stack,
            )
            try:
                sock.sendall(line.encode())
            except OSError:
                # greenlane went away (exit / detach): remove ourselves so the
                # target stops paying the trace cost, and restore any prior hook.
                try:
                    sock.close()
                except OSError:
                    pass
                greenlet.settrace(prev)
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
    _gc_start = [0]

    def _gc_cb(phase, info):
        if phase == "start":
            _gc_start[0] = time.perf_counter_ns()
        elif phase == "stop":
            now = time.perf_counter_ns()
            line = "gc\t%d\t%d\t%d\t%d\n" % (
                _gc_start[0] - t0,                 # start, relative to trace t0
                now - _gc_start[0],                # pause duration (ns)
                info.get("generation", -1),
                info.get("collected", 0),
            )
            try:
                sock.sendall(line.encode())
            except OSError:
                try:
                    gc.callbacks.remove(_gc_cb)    # greenlane gone: stop tracking
                except ValueError:
                    pass

    gc.callbacks.append(_gc_cb)

    # Keep references alive past this function's scope so they aren't GC'd.
    import builtins
    builtins.__greenlane_sock = sock
    builtins.__greenlane_cb = _cb
    builtins.__greenlane_gc = _gc_cb


_greenlane_install()
