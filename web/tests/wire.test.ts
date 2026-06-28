import { expect, test } from "bun:test";
import { decodeWindow, formatBytes, formatRate } from "../src/wire.ts";

// Build a binary `window` frame exactly as the Rust server's `encode_window`
// does, so decoding it round-trips the layout (the server↔viewer contract):
//   u32 LE headerLen | header JSON | pad→4 | f32 start | f32 dur | u32 trk
// Render-only: 3 columns, 12 bytes/slice. func/task/stack are fetched lazily via
// the `detail` request, so there's no dict / func-task-stack columns here.
function buildFrame(
  header: object,
  cols: { start: number[]; dur: number[]; trk: number[] },
): ArrayBuffer {
  const hbytes = new TextEncoder().encode(JSON.stringify(header));
  const off = (4 + hbytes.length + 3) & ~3; // columns start 4-byte aligned
  const n = cols.start.length;
  const buf = new ArrayBuffer(off + n * 12);
  new DataView(buf).setUint32(0, hbytes.length, true);
  new Uint8Array(buf, 4, hbytes.length).set(hbytes);
  let p = off;
  const put = (
    Ctor: typeof Float32Array | typeof Uint32Array,
    arr: number[],
  ) => {
    new Ctor(buf, p, arr.length).set(arr);
    p += arr.length * 4;
  };
  put(Float32Array, cols.start);
  put(Float32Array, cols.dur);
  put(Uint32Array, cols.trk);
  return buf;
}

const HEADER = {
  type: "window",
  n: 2,
  t0: 0,
  t1: 6,
  req: 5,
  counts: { visible: 2, total: 42 },
  capped: false,
  spanNs: 6_000_000,
  bytes: 100,
  tracks: [{ gid: 10, name: "Greenlet-1", isHub: false, runNs: 5 }],
  gc: [{ start: 1, dur: 2, gen: 0, collected: 4 }],
};

test("decodeWindow round-trips header fields and columns", () => {
  const buf = buildFrame(HEADER, { start: [0, 5], dur: [5, 1], trk: [0, 0] });
  const w = decodeWindow(buf);
  expect(w.req).toBe(5);
  expect(w.t0).toBe(0);
  expect(w.t1).toBe(6);
  expect(w.total).toBe(42);
  expect(w.capped).toBe(false);
  expect(w.tracks).toHaveLength(1);
  expect(w.tracks[0].name).toBe("Greenlet-1");
  expect(w.gc[0].collected).toBe(4);
  expect(Array.from(w.startMs)).toEqual([0, 5]);
  expect(Array.from(w.durMs)).toEqual([5, 1]);
  expect(Array.from(w.trackIdx)).toEqual([0, 0]);
  expect(w.startMs).toHaveLength(2);
});

test("decodeWindow reads the data bounds + retention horizon", () => {
  const buf = buildFrame(
    { ...HEADER, minStart: 1_000, maxEnd: 6_000_000, retainedFromNs: 500 },
    { start: [0, 5], dur: [5, 1], trk: [0, 0] },
  );
  const w = decodeWindow(buf);
  expect(w.minStart).toBe(1_000);
  expect(w.maxEnd).toBe(6_000_000);
  expect(w.retainedFromNs).toBe(500);
});

test("decodeWindow defaults bounds/retention to 0 when absent", () => {
  const buf = buildFrame(HEADER, { start: [0, 5], dur: [5, 1], trk: [0, 0] });
  const w = decodeWindow(buf);
  expect(w.minStart).toBe(0);
  expect(w.maxEnd).toBe(0);
  expect(w.retainedFromNs).toBe(0);
});

test("decodeWindow rejects an out-of-range track index", () => {
  // HEADER has a single track (index 0 valid); claim trackIdx 1 for one row.
  const buf = buildFrame(HEADER, { start: [0, 5], dur: [5, 1], trk: [0, 1] });
  expect(() => decodeWindow(buf)).toThrow(/trackIdx\[1\]=1 out of range/);
});

test("decodeWindow handles an empty window (n=0)", () => {
  const buf = buildFrame(
    { ...HEADER, n: 0, counts: { visible: 0, total: 0 }, tracks: [], gc: [] },
    { start: [], dur: [], trk: [] },
  );
  const w = decodeWindow(buf);
  expect(w.startMs).toHaveLength(0);
  expect(w.tracks).toHaveLength(0);
});

test("decodeWindow rejects a truncated frame instead of reading OOB", () => {
  // Claim n=8 in the header but only allocate room for the columns of 1 row.
  const header = {
    ...HEADER,
    n: 8,
    counts: { visible: 8, total: 8 },
  };
  const hbytes = new TextEncoder().encode(JSON.stringify(header));
  const off = (4 + hbytes.length + 3) & ~3;
  const buf = new ArrayBuffer(off + 12); // room for 1 row, not 8
  new DataView(buf).setUint32(0, hbytes.length, true);
  new Uint8Array(buf, 4, hbytes.length).set(hbytes);
  expect(() => decodeWindow(buf)).toThrow(/malformed window frame/);
});

test("formatBytes scales units and trims precision", () => {
  expect(formatBytes(512)).toBe("512 B");
  expect(formatBytes(1024)).toBe("1.0 KB");
  expect(formatBytes(1536)).toBe("1.5 KB");
  expect(formatBytes(1024 * 1024)).toBe("1.0 MB");
  expect(formatBytes(20 * 1024 * 1024)).toBe("20 MB"); // ≥10 → no decimal
  expect(formatBytes(3 * 1024 ** 3)).toBe("3.0 GB");
});

test("formatRate compacts thousands", () => {
  expect(formatRate(5)).toBe("5.0/s");
  expect(formatRate(42)).toBe("42/s");
  expect(formatRate(1500)).toBe("1.5k/s");
  expect(formatRate(12_000)).toBe("12.0k/s");
});
