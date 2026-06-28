# greenlane binary trace encoder — the single Python source of truth, inlined
# into both bootstraps at materialization (see `__GLR_ENCODER__` in src/main.rs).
# It mirrors the Rust decoder in src/trace_format.rs byte-for-byte; the two MUST
# change together. Kept import-light and allocation-conscious for the hot path.
#
# Frame stream:  Header(magic b"GLR\0", version 1) then  Tag(1) Len(varint) Body.
# See docs/design/glr-format.md for the full spec.


def _wv(buf, v):
    # Append an unsigned LEB128 varint.
    while True:
        b = v & 0x7F
        v >>= 7
        if v:
            buf.append(b | 0x80)
        else:
            buf.append(b)
            return


def _zz(v):
    # Zigzag a signed int to unsigned (masked to 64 bits) for varint encoding.
    return ((v << 1) ^ (v >> 63)) & 0xFFFFFFFFFFFFFFFF


class GlrEnc:
    """Builds the framed binary stream into a shared `bytearray` (the bootstrap's
    send buffer). Interns strings + stacks and delta-encodes timestamps exactly as
    the Rust decoder expects."""

    __slots__ = ("buf", "_si", "_ki", "_ns", "_nk", "base")

    # Frame tags / field types — must match src/trace_format.rs.
    _T_SCHEMA = 0x01
    _T_EVENT = 0x02
    _T_STRPOOL = 0x03
    _T_STACKPOOL = 0x04
    _T_TSRESET = 0x05
    _T_META = 0x06
    _FT_VARINT = 0x01
    _FT_I64 = 0x02
    _FT_PSTR = 0x03
    _FT_PSTACK = 0x04
    _FLAG_TS = 0x01
    _UNIT_NS = 1
    _TY_SWITCH = 1
    _TY_GC = 2

    def __init__(self, buf):
        self.buf = buf
        self._si = {"": 0}  # string -> pool id (0 = "")
        self._ki = {"": 0}  # stack key -> pool id (0 = empty stack)
        self._ns = 1
        self._nk = 1
        self.base = 0
        buf += b"GLR\x00\x01"  # Header: magic + version

    def _frame(self, tag, body):
        b = self.buf
        b.append(tag)
        _wv(b, len(body))
        b += body

    def str_id(self, s):
        # Intern a string; emit a String-pool frame on first sight. Returns its id.
        i = self._si.get(s)
        if i is not None:
            return i
        i = self._ns
        self._ns = i + 1
        self._si[s] = i
        body = bytearray()
        bb = s.encode("utf-8", "replace")
        _wv(body, len(bb))
        body += bb
        self._frame(self._T_STRPOOL, body)
        return i

    def stack_id(self, frames):
        # Intern a stack (list of frame strings, leaf -> root). Emits the frames'
        # String-pool entries first, then a Stack-pool frame. Returns its id; an
        # empty stack is id 0 (no frame emitted).
        if not frames:
            return 0
        key = "\x00".join(frames)
        i = self._ki.get(key)
        if i is not None:
            return i
        ids = [self.str_id(f) for f in frames]  # must precede the stack frame
        i = self._nk
        self._nk = i + 1
        self._ki[key] = i
        body = bytearray()
        _wv(body, len(ids))
        for fid in ids:
            _wv(body, fid)
        self._frame(self._T_STACKPOOL, body)
        return i

    def schema(self, type_id, has_ts, name, fields):
        # fields: list of (name, field_type_tag, unit).
        body = bytearray()
        body += type_id.to_bytes(2, "little")
        body.append(self._FLAG_TS if has_ts else 0)
        nb = name.encode()
        _wv(body, len(nb))
        body += nb
        _wv(body, len(fields))
        for fn, ft, unit in fields:
            fb = fn.encode()
            _wv(body, len(fb))
            body += fb
            body.append(ft)
            body.append(unit)
        self._frame(self._T_SCHEMA, body)

    def write_wire_schemas(self):
        # The two event types the wire uses. Field order MUST match the Rust
        # decoder's builders (src/trace_format.rs).
        self.schema(
            self._TY_SWITCH,
            True,
            "switch",
            [
                ("target", self._FT_VARINT, 0),
                ("label", self._FT_PSTR, 0),
                ("func", self._FT_PSTR, 0),
                ("task", self._FT_PSTR, 0),
                ("stack", self._FT_PSTACK, 0),
                ("thread", self._FT_VARINT, 0),
            ],
        )
        self.schema(
            self._TY_GC,
            False,
            "gc",
            [
                ("start", self._FT_VARINT, self._UNIT_NS),
                ("dur", self._FT_VARINT, self._UNIT_NS),
                ("generation", self._FT_I64, 0),
                ("collected", self._FT_I64, 0),
            ],
        )

    def meta(self, epoch_ms, tid, pid, pyinfo):
        body = bytearray()
        _wv(body, epoch_ms)
        _wv(body, tid)
        _wv(body, _zz(pid))
        pb = pyinfo.encode("utf-8", "replace")
        _wv(body, len(pb))
        body += pb
        self._frame(self._T_META, body)

    def _ts_delta(self, ts):
        # 24-bit delta since the running base, or a Ts-reset frame + 0 on overflow.
        delta = ts - self.base
        if delta < 0 or delta > 0xFFFFFF:
            self._frame(self._T_TSRESET, ts.to_bytes(8, "little"))
            self.base = ts
            return 0
        self.base = ts
        return delta

    def switch(self, ts, target, label_id, func_id, task_id, stack_id, thread):
        # Caller interns label/func/stack first (str_id/stack_id), so their pool
        # frames precede this event.
        d = self._ts_delta(ts)
        body = bytearray()
        body += b"\x01\x00"  # TY_SWITCH (u16 LE)
        body.append(d & 0xFF)
        body.append((d >> 8) & 0xFF)
        body.append((d >> 16) & 0xFF)
        _wv(body, target)
        _wv(body, label_id)
        _wv(body, func_id)
        _wv(body, task_id)
        _wv(body, stack_id)
        _wv(body, thread)
        self._frame(self._T_EVENT, body)

    def gc(self, start, dur, generation, collected):
        body = bytearray()
        body += b"\x02\x00"  # TY_GC (u16 LE)
        _wv(body, start)
        _wv(body, dur)
        _wv(body, _zz(generation))
        _wv(body, _zz(collected))
        self._frame(self._T_EVENT, body)
