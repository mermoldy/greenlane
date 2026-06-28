"""Behavioral unit tests for the binary trace encoder ``src/glr.py``.

``glr.py`` is the single Python source of truth for the ``.glr`` wire format; it is
inlined into ``src/bootstrap.py`` at materialization and must stay byte-compatible
with the Rust decoder in ``src/trace_format.rs``. These tests exercise the encoder
directly (it is pure stdlib, no gevent/greenlet) and round-trip its output through a
small decoder defined here — mirroring the ``#[cfg(test)]`` suite in
``src/trace_format.rs`` (notably ``python_encoder_matches_rust_decoder``), but
without needing a Rust toolchain.
"""

import glr

# Frame tags / field types, kept independent of the encoder's private constants so a
# silent change to either side is caught here. Must match src/trace_format.rs.
T_SCHEMA = 0x01
T_EVENT = 0x02
T_STRPOOL = 0x03
T_STACKPOOL = 0x04
T_TSRESET = 0x05
T_META = 0x06
FT_PSTR = 0x03
FT_PSTACK = 0x04
TY_SWITCH = 1
TY_GC = 2


# ── A reference decoder (mirror of src/trace_format.rs) ──────────────────────────


def _unzigzag(v):
    return (v >> 1) ^ -(v & 1)


class _Reader:
    """Cursor over a frame body: LEB128 varints, fixed-width ints, inline strings."""

    def __init__(self, body):
        self.body = body
        self.pos = 0

    def u8(self):
        b = self.body[self.pos]
        self.pos += 1
        return b

    def take(self, n):
        s = self.body[self.pos : self.pos + n]
        assert len(s) == n, "frame body truncated"
        self.pos += n
        return s

    def u16(self):
        b = self.take(2)
        return b[0] | (b[1] << 8)

    def u24(self):
        b = self.take(3)
        return b[0] | (b[1] << 8) | (b[2] << 16)

    def varint(self):
        result = 0
        shift = 0
        while True:
            b = self.u8()
            result |= (b & 0x7F) << shift
            if not (b & 0x80):
                return result
            shift += 7

    def ivarint(self):
        return _unzigzag(self.varint())

    def istr(self):
        n = self.varint()
        return self.take(n).decode("utf-8")


class Decoder:
    """Decodes a full ``glr`` byte stream into a list of ``(kind, fields)`` items.

    Maintains the string/stack pools, the schema registry, and the running 24-bit
    timestamp base exactly as the Rust decoder does, so events delta-decode and
    string/stack ids resolve to their interned values.
    """

    def __init__(self):
        self.strings = [""]
        self.stacks = [""]
        self.schemas = {}  # type_id -> (has_ts, [field_type, ...])
        self.base = 0

    def decode(self, buf):
        assert bytes(buf[:5]) == b"GLR\x00\x01", "bad header"
        pos = 5
        items = []
        while pos < len(buf):
            tag = buf[pos]
            pos += 1
            r = _Reader(buf)
            r.pos = pos
            length = r.varint()
            pos = r.pos
            body = buf[pos : pos + length]
            assert len(body) == length, "frame truncated"
            pos += length
            item = self._frame(tag, bytes(body))
            if item is not None:
                items.append(item)
        return items

    def _frame(self, tag, body):
        r = _Reader(body)
        if tag == T_STRPOOL:
            self.strings.append(r.istr())
            return None
        if tag == T_STACKPOOL:
            n = r.varint()
            frames = [self.strings[r.varint()] for _ in range(n)]
            self.stacks.append(" <- ".join(frames))
            return None
        if tag == T_TSRESET:
            self.base = int.from_bytes(r.take(8), "little")
            return None
        if tag == T_META:
            epoch_ms = r.varint()
            tid = r.varint()
            pid = r.ivarint()
            pyinfo = r.istr()
            return ("meta", {"epoch_ms": epoch_ms, "tid": tid, "pid": pid, "pyinfo": pyinfo})
        if tag == T_SCHEMA:
            return self._schema(r)
        if tag == T_EVENT:
            return self._event(r)
        return None  # unknown tag — length prefix lets a real reader skip it

    def _schema(self, r):
        type_id = r.u16()
        flags = r.u8()
        r.istr()  # name (unused here)
        nfields = r.varint()
        field_types = []
        for _ in range(nfields):
            r.istr()  # field name
            field_types.append(r.u8())
            r.u8()  # unit
        self.schemas[type_id] = (flags & 0x01 != 0, field_types)
        return None

    def _event(self, r):
        type_id = r.u16()
        has_ts, field_types = self.schemas[type_id]
        ts = 0
        if has_ts:
            self.base += r.u24()
            ts = self.base
        vals = []
        for ft in field_types:
            if ft == FT_PSTR:
                vals.append(self.strings[r.varint()])
            elif ft == FT_PSTACK:
                vals.append(self.stacks[r.varint()])
            elif ft == 0x02:  # FT_I64
                vals.append(r.ivarint())
            else:  # FT_VARINT
                vals.append(r.varint())
        if type_id == TY_SWITCH:
            target, label, func, task, stack, thread = vals
            return (
                "switch",
                {
                    "ts": ts,
                    "target": target,
                    "label": label,
                    "func": func,
                    "task": task,
                    "stack": stack,
                    "thread": thread,
                },
            )
        if type_id == TY_GC:
            start, dur, generation, collected = vals
            return (
                "gc",
                {"start": start, "dur": dur, "generation": generation, "collected": collected},
            )
        return None


def _frames(buf):
    """Split a stream (past the 5-byte header) into ``(tag, body)`` pairs."""
    pos = 5
    out = []
    while pos < len(buf):
        tag = buf[pos]
        pos += 1
        r = _Reader(buf)
        r.pos = pos
        length = r.varint()
        pos = r.pos
        out.append((tag, bytes(buf[pos : pos + length])))
        pos += length
    return out


# ── Primitives ───────────────────────────────────────────────────────────────


def _wv(v):
    buf = bytearray()
    glr._wv(buf, v)
    return bytes(buf)


def test_wv_known_encodings():
    assert _wv(0) == b"\x00"
    assert _wv(1) == b"\x01"
    assert _wv(127) == b"\x7f"
    assert _wv(128) == b"\x80\x01"
    assert _wv(300) == b"\xac\x02"
    assert _wv(16383) == b"\xff\x7f"
    assert _wv(16384) == b"\x80\x80\x01"


def test_wv_roundtrips_including_64bit():
    for v in [0, 1, 127, 128, 300, 16_383, 16_384, 2**32 - 1, 2**64 - 1]:
        assert _Reader(_wv(v)).varint() == v


def test_zz_known_and_roundtrip():
    assert glr._zz(0) == 0
    assert glr._zz(-1) == 1
    assert glr._zz(1) == 2
    assert glr._zz(-2) == 3
    assert glr._zz(2) == 4
    for v in [0, -1, 1, -2, 2, -12345, 12345, -(2**63), 2**63 - 1]:
        assert _unzigzag(glr._zz(v)) == v


# ── Header & interning ─────────────────────────────────────────────────────────


def test_constructor_writes_header():
    buf = bytearray()
    glr.GlrEnc(buf)
    assert bytes(buf) == b"GLR\x00\x01"


def test_str_id_interns_and_dedups():
    buf = bytearray()
    enc = glr.GlrEnc(buf)

    # The empty string is pre-seeded as id 0 and emits no frame.
    assert enc.str_id("") == 0
    assert _frames(buf) == []

    assert enc.str_id("hello") == 1
    frames = _frames(buf)
    assert len(frames) == 1
    tag, body = frames[0]
    assert tag == T_STRPOOL
    assert body == b"\x05hello"

    # Same string → same id, no new frame.
    assert enc.str_id("hello") == 1
    assert len(_frames(buf)) == 1

    # A new string gets the next id and one more frame.
    assert enc.str_id("world") == 2
    assert len(_frames(buf)) == 2


def test_stack_id_interns_frames_then_dedups():
    buf = bytearray()
    enc = glr.GlrEnc(buf)

    # Empty stack is id 0 and emits nothing.
    assert enc.stack_id([]) == 0
    assert _frames(buf) == []

    sid = enc.stack_id(["leaf", "root"])
    assert sid == 1
    tags = [t for t, _ in _frames(buf)]
    # Two string-pool frames (leaf, root) must precede the stack-pool frame.
    assert tags == [T_STRPOOL, T_STRPOOL, T_STACKPOOL]

    # Same stack → same id, no new frames.
    assert enc.stack_id(["leaf", "root"]) == 1
    assert len([t for t, _ in _frames(buf)]) == 3

    # The decoder resolves the stack to its joined frame string.
    items = Decoder()
    items.strings = ["", "leaf", "root"]
    # Re-decode the stack-pool body to confirm join order (leaf -> root).
    stack_body = [b for t, b in _frames(buf) if t == T_STACKPOOL][0]
    items._frame(T_STACKPOOL, stack_body)
    assert items.stacks[-1] == "leaf <- root"


# ── Frame-level round trips ──────────────────────────────────────────────────


def test_write_wire_schemas_registers_switch_and_gc():
    buf = bytearray()
    enc = glr.GlrEnc(buf)
    enc.write_wire_schemas()
    dec = Decoder()
    dec.decode(buf)  # populates dec.schemas as a side effect
    assert set(dec.schemas) == {TY_SWITCH, TY_GC}
    assert dec.schemas[TY_SWITCH][0] is True  # switch carries a timestamp
    assert dec.schemas[TY_GC][0] is False  # gc does not


def test_meta_roundtrip_with_negative_pid():
    buf = bytearray()
    enc = glr.GlrEnc(buf)
    enc.meta(1700, 42, -99, '{"runtime":"gevent"}')
    ((kind, m),) = Decoder().decode(buf)
    assert kind == "meta"
    assert m == {"epoch_ms": 1700, "tid": 42, "pid": -99, "pyinfo": '{"runtime":"gevent"}'}


def test_switch_roundtrip():
    buf = bytearray()
    enc = glr.GlrEnc(buf)
    enc.write_wire_schemas()
    enc.switch(
        1000,
        0x55,
        enc.str_id("Greenlet-3"),
        enc.str_id("app.py:run:9"),
        enc.str_id("req-7"),
        enc.stack_id(["app.py:run:9", "app.py:main:2"]),
        7,
    )
    switches = [f for k, f in Decoder().decode(buf) if k == "switch"]
    assert switches == [
        {
            "ts": 1000,
            "target": 0x55,
            "label": "Greenlet-3",
            "func": "app.py:run:9",
            "task": "req-7",
            "stack": "app.py:run:9 <- app.py:main:2",
            "thread": 7,
        }
    ]


def test_switch_ts_delta_reset_on_large_gap():
    buf = bytearray()
    enc = glr.GlrEnc(buf)
    enc.write_wire_schemas()
    enc.switch(100, 0xAA, enc.str_id("Greenlet-1"), 0, 0, 0, 7)
    # A jump past the 24-bit delta range must emit a Ts-reset frame.
    far = 100 + (1 << 25)
    enc.switch(far, 0xBB, enc.str_id("Hub"), 0, 0, 0, 9)

    assert any(t == T_TSRESET for t, _ in _frames(buf)), "large gap should force a Ts-reset"

    switches = [f for k, f in Decoder().decode(buf) if k == "switch"]
    assert [s["ts"] for s in switches] == [100, far]  # absolute ts reconstructed
    assert [s["label"] for s in switches] == ["Greenlet-1", "Hub"]


def test_gc_roundtrip_with_negative_fields():
    buf = bytearray()
    enc = glr.GlrEnc(buf)
    enc.write_wire_schemas()
    enc.gc(500, 9, -1, -7)
    gcs = [f for k, f in Decoder().decode(buf) if k == "gc"]
    assert gcs == [{"start": 500, "dur": 9, "generation": -1, "collected": -7}]


def test_full_stream_roundtrip():
    """Schemas + meta + two switches (dedup + ts-reset) + gc, mirroring the Rust
    cross-language test ``python_encoder_matches_rust_decoder``."""
    buf = bytearray()
    enc = glr.GlrEnc(buf)
    enc.write_wire_schemas()
    enc.meta(1700, 42, 99, '{"runtime":"gevent"}')
    lid = enc.str_id("Greenlet-3")
    fid = enc.str_id("app.py:run:9")
    tid = enc.str_id("req-7")
    sid = enc.stack_id(["app.py:run:9", "app.py:main:2"])
    enc.switch(1000, 0x55, lid, fid, tid, sid, 7)
    # Second switch reuses interned strings/stack ("Hub" is new) and jumps far
    # enough to force a Ts-reset.
    enc.switch(1000 + (1 << 25), 0x66, enc.str_id("Hub"), 0, 0, 0, 9)
    enc.gc(500, 9, 2, 7)

    items = Decoder().decode(buf)
    kinds = [k for k, _ in items]
    assert kinds == ["meta", "switch", "switch", "gc"]

    assert items[0][1]["pid"] == 99
    assert items[1][1] == {
        "ts": 1000,
        "target": 0x55,
        "label": "Greenlet-3",
        "func": "app.py:run:9",
        "task": "req-7",
        "stack": "app.py:run:9 <- app.py:main:2",
        "thread": 7,
    }
    assert items[2][1]["ts"] == 1000 + (1 << 25)
    assert items[2][1]["label"] == "Hub"
    assert items[2][1]["func"] == ""  # id 0
    assert items[2][1]["stack"] == ""  # id 0
    assert items[3][1] == {"start": 500, "dur": 9, "generation": 2, "collected": 7}
