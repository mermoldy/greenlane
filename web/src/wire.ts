// Wire format + small formatting helpers shared by the app and its tests.
//
// Kept separate from main.tsx (which renders the app on import) so these pure
// functions — especially the binary `window` frame decoder, the server↔viewer
// contract — can be unit-tested without spinning up the DOM.

export type GcEvent = {
  start: number;
  dur: number;
  gen: number;
  collected: number;
};
export type WindowTrack = {
  gid: number;
  name: string;
  isHub: boolean;
  runNs: number;
};

// Bytes → a compact human size, e.g. 1.4 MB.
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n,
    i = -1;
  do {
    v /= 1024;
    i++;
  } while (v >= 1024 && i < units.length - 1);
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

// Events/sec → a short label (e.g. 1.2k/s) for the header stat.
export function formatRate(r: number): string {
  if (r >= 1000) return `${(r / 1000).toFixed(1)}k/s`;
  return `${r.toFixed(r < 10 ? 1 : 0)}/s`;
}

// Decode the server's binary `window` frame (render-only, 12 bytes/execution):
//   u32 LE headerLen | header JSON | pad→4 | f32 startMs[n] | f32 durMs[n] |
//   u32 trackIdx[n]
// Typed-array views point straight into the received buffer — no copy. A execution's
// func/task/stack are NOT in the frame; the viewer fetches them lazily on hover
// via the `detail` request. NOTE: `startMs` is **relative to the window's `t0`**
// (small values → f32 keeps sub-ms precision on long captures); the timeline adds
// back `t0 - origin` in f64 to get absolute ms.
export function decodeWindow(buf: ArrayBuffer) {
  const headerLen = new DataView(buf).getUint32(0, true);
  const header = JSON.parse(
    new TextDecoder().decode(new Uint8Array(buf, 4, headerLen)),
  );
  const n: number = header.n;
  let off = (4 + headerLen + 3) & ~3; // columns start 4-byte aligned
  // Validate the frame is long enough for all 3 columns before making views —
  // a truncated/corrupt frame must not throw an opaque out-of-bounds error.
  if (!(n >= 0) || off + n * 12 > buf.byteLength) {
    throw new Error(`malformed window frame (n=${n}, len=${buf.byteLength})`);
  }
  const col = (Ctor: typeof Float32Array | typeof Uint32Array) => {
    const a = new Ctor(buf, off, n);
    off += n * 4;
    return a;
  };
  const startMs = col(Float32Array) as Float32Array;
  const durMs = col(Float32Array) as Float32Array;
  const trackIdx = col(Uint32Array) as Uint32Array;
  const tracks = header.tracks as WindowTrack[];
  // Validate trackIdx is in range BEFORE the caller applies the frame: an
  // out-of-range index would otherwise throw deep inside the renderer, outside
  // this decode's try/catch. Cheap one pass over the (window-capped) column.
  checkIndices(trackIdx, tracks.length, "trackIdx");
  return {
    req: (header.req as number) ?? 0,
    t0: header.t0 as number,
    t1: header.t1 as number,
    // Absolute ns bounds of the data actually returned (0 when empty). `maxStart` is
    // the viewer's next live-follow data frontier (the `from` for its next tail).
    minStart: (header.minStart as number) ?? 0,
    maxStart: (header.maxStart as number) ?? 0,
    maxEnd: (header.maxEnd as number) ?? 0,
    // Live retention horizon (ns): data before this was evicted (0 = none).
    retainedFromNs: (header.retainedFromNs as number) ?? 0,
    // True when this frame is the new tail of a live-follow request (append to the
    // existing buffer); false/absent for a full window (replace).
    append: (header.append as boolean) ?? false,
    tracks,
    gc: header.gc as GcEvent[],
    bytes: header.bytes as number,
    total: header.counts.total as number,
    capped: header.capped as boolean,
    // Whether the timeline is in start-sorted order; the viewer only uses the
    // append fast path while true (multi-thread → out-of-order → must full-load).
    sorted: (header.sorted as boolean) ?? true,
    spanNs: header.spanNs as number,
    startMs,
    durMs,
    trackIdx,
  };
}

// Throw if any value in `idx` is outside [0, len). Empty columns (len 0) are only
// valid when there are no rows to index.
function checkIndices(idx: Uint32Array, len: number, what: string) {
  for (let i = 0; i < idx.length; i++) {
    if (idx[i] >= len) {
      throw new Error(
        `malformed window frame: ${what}[${i}]=${idx[i]} out of range (len ${len})`,
      );
    }
  }
}
