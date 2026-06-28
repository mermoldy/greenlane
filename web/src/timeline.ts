// WebGL2 instanced-quad timeline renderer + 2D axis/grid overlay.
//
// One greenlet run-interval = one instanced rectangle whose WIDTH is its real
// duration (dur * pxPerMs). Pan/zoom are uniform changes only — geometry is
// never rebuilt on the CPU per frame. New slices append with bufferSubData; the
// buffers are preallocated large and grow (rarely) via GPU-to-GPU copies, so
// there are no periodic CPU→GPU re-upload stalls.
//
// Track display order is decoupled from insert order via a track→row lookup
// texture (Hub on top, greenlets by ident), so reordering never touches the
// per-slice buffers.
//
// Known v1 simplifications (deliberate seams for later):
//  - Times are f32 milliseconds relative to trace start (~µs precision over
//    minutes). For long captures, rebase the origin or use a server LOD query.
//  - Picking is a linear scan (fine to ~1e5 visible). Per-track index later.
//  - No LOD yet: zoomed all the way out, sub-pixel slices hit the 1px floor.

export interface Slice {
  gid: number;
  start: number; // ns since trace start (from server)
  dur: number; // ns
  name: string;
  func: string; // app function the greenlet resumed into
  task: string; // app correlation id (request_id/task_id), or ""
  stack: string; // call chain (leaf → root, " <- " joined)
}

export interface Hover {
  name: string;
  gid: number;
  startNs: number;
  durNs: number;
  func: string;
  task: string;
  stack: string;
  x: number; // screen px (for tooltip placement)
  y: number;
}

const MIN_PX = 1; // smallest rendered slice width; proportional above this
const MAX_PXPERMS = 1000; // max zoom = 1px per 1µs
const MIN_PXPERMS = 1 / 200; // min zoom = 1px per 200ms
const ZOOM_SENS = 0.0008; // wheel-zoom sensitivity (lower = finer/slower)
const RULER_H = 22; // top time-ruler band, CSS px
const CPU_H = 60; // CPU graph band under the ruler, CSS px
const GAP_H = 7; // gap between the CPU band and the first track
const HEADER_H = RULER_H + CPU_H + GAP_H; // tracks begin below this
const AXIS_X = 6; // shared left inset for axis labels (CPU % and track names)
const INIT_CAP = 1 << 18; // ~262k slices preallocated (cheap grows after)
const WARN_MS = 20; // spans longer than this get a yellow border
const SLOW_MS = 50; // spans longer than this get a red border
const BIN_MS = 1; // CPU histogram resolution: non-Hub run-time per 1ms bin
const TEX_W = 4096; // row-lookup texture width; 2D-wrapped to scale past 16k tracks
// Shared axis font — matches the DOM track labels for consistent styling.
const AXIS_FONT = "11px ui-monospace, 'SF Mono', Menlo, monospace";

/** Adaptive time formatting: ns / µs / ms / s by magnitude. Input is ms. */
export function formatTime(ms: number): string {
  const a = Math.abs(ms);
  if (a < 1e-3) return `${(ms * 1e6).toFixed(0)} ns`;
  if (a < 1) return `${(ms * 1e3).toFixed(a < 0.1 ? 1 : 0)} µs`;
  if (a < 1e3) return `${ms.toFixed(a < 10 ? 2 : 1)} ms`;
  return `${(ms / 1e3).toFixed(2)} s`;
}

/** Round a raw step up to a "nice" 1/2/5 × 10ⁿ value. */
function niceStep(raw: number): number {
  const p = Math.pow(10, Math.floor(Math.log10(raw)));
  const f = raw / p;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 5 ? 5 : 10) * p;
}

// Stable display ordering: Hub(s) on top, then greenlets by ident, then rest.
function trackRank(name: string): [number, number, string] {
  if (/^hub/i.test(name)) {
    const n = parseInt(name.split("-")[1] ?? "0", 10);
    return [0, isNaN(n) ? 0 : n, name];
  }
  const m = name.match(/^greenlet-(\d+)/i);
  if (m) return [1, parseInt(m[1], 10), name];
  if (/^greenlet$/i.test(name)) return [2, 0, name];
  return [3, 0, name];
}

const VERT = `#version 300 es
layout(location=0) in vec2 a_corner;
layout(location=1) in float a_start;   // ms relative to trace start
layout(location=2) in float a_dur;     // ms
layout(location=3) in float a_track;   // stable track id
layout(location=4) in vec3 a_color;
layout(location=5) in float a_slow;    // 1 = highlight (slow, non-Hub)
uniform vec2 u_res;
uniform float u_viewT0;
uniform float u_pxPerMs;
uniform float u_trackH;
uniform float u_scrollY;
uniform float u_rulerH;
uniform float u_minPx;
uniform float u_texW;              // row-lookup texture width
uniform highp sampler2D u_rowTex;  // track id -> display row (2D-wrapped)
out vec3 v_color;
out vec2 v_local;    // 0..1 within the quad
out vec2 v_sizePx;   // quad size in device px (for border thickness)
out float v_slow;
void main() {
  int ti = int(a_track + 0.5);
  int tw = int(u_texW);
  float row = texelFetch(u_rowTex, ivec2(ti % tw, ti / tw), 0).r;
  float w = max(a_dur * u_pxPerMs, u_minPx);      // WIDTH == duration
  float h = u_trackH - 1.0;
  float x = (a_start - u_viewT0) * u_pxPerMs + a_corner.x * w;
  float y = u_rulerH + row * u_trackH - u_scrollY + a_corner.y * h;
  vec2 clip = vec2(x / u_res.x * 2.0 - 1.0, 1.0 - y / u_res.y * 2.0);
  gl_Position = vec4(clip, 0.0, 1.0);
  v_color = a_color;             // original track color preserved
  v_local = a_corner;
  v_sizePx = vec2(w, h);
  v_slow = a_slow;
}`;

const FRAG = `#version 300 es
precision mediump float;
in vec3 v_color;
in vec2 v_local;
in vec2 v_sizePx;
in float v_slow;
out vec4 o_color;
void main() {
  vec3 col = v_color;
  if (v_slow > 0.5) {
    // Border highlight, keeping the fill in the track's own color.
    // v_slow: 1 = warn (>20ms, yellow), 2 = slow (>50ms, red).
    float dx = min(v_local.x, 1.0 - v_local.x) * v_sizePx.x;
    float dy = min(v_local.y, 1.0 - v_local.y) * v_sizePx.y;
    if (min(dx, dy) < 2.0) {
      col = v_slow > 1.5 ? vec3(0.92, 0.22, 0.27) : vec3(0.92, 0.80, 0.36);
    }
  }
  o_color = vec4(col, 1.0);
}`;

function compile(
  gl: WebGL2RenderingContext,
  type: number,
  src: string,
): WebGLShader {
  const s = gl.createShader(type)!;
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) {
    throw new Error("shader: " + gl.getShaderInfoLog(s));
  }
  return s;
}

// The Hub's reserved color (theme green). Only the Hub is green; only slow
// spans are red — so greenlet hues are mapped to bands that avoid both.
const HUB_COLOR: [number, number, number] = [163 / 255, 190 / 255, 140 / 255];

function trackColor(i: number): [number, number, number] {
  // Golden-angle spread, but remapped into allowed hue bands: orange/yellow
  // [25,80) and cyan/blue/purple/magenta [165,330) — skipping red (~0/360)
  // and green (~95-155).
  const t = (i * 0.6180339887) % 1;
  const seg1 = 55,
    seg2 = 165; // band widths
  const pos = t * (seg1 + seg2);
  const h = pos < seg1 ? 25 + pos : 165 + (pos - seg1);
  return hsl(h, 0.55, 0.6);
}
function hsl(h: number, s: number, l: number): [number, number, number] {
  const c = (1 - Math.abs(2 * l - 1)) * s;
  const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
  const m = l - c / 2;
  const [r, g, b] =
    h < 60
      ? [c, x, 0]
      : h < 120
        ? [x, c, 0]
        : h < 180
          ? [0, c, x]
          : h < 240
            ? [0, x, c]
            : h < 300
              ? [x, 0, c]
              : [c, 0, x];
  return [r + m, g + m, b + m];
}

interface Attr {
  buf: WebGLBuffer;
  loc: number;
  size: number;
}

export type SortMode =
  "ident" | "activity" | "recent1" | "recent10" | "recent60";

export class Timeline {
  private gl: WebGL2RenderingContext;
  private ctx: CanvasRenderingContext2D;
  private prog: WebGLProgram;
  private vao: WebGLVertexArrayObject;
  private u: Record<string, WebGLUniformLocation | null> = {};

  // Instance attributes (GPU) + their CPU mirrors.
  private aStart: Attr;
  private aDur: Attr;
  private aTrack: Attr;
  private aColor: Attr;
  private aSlow: Attr;
  private cap = 0;
  count = 0;
  // Max observed event end-time (ms since t0) — the capture's wall span, used
  // for the events/sec rate. Tracked incrementally so it costs nothing.
  private spanMs = 0;
  private cStart = new Float32Array(0);
  private cDur = new Float32Array(0);
  private cTrack = new Float32Array(0);
  private cColor = new Float32Array(0);
  private cSlow = new Float32Array(0);
  private slowIdx: number[] = []; // indices of warn/slow spans, for the slow log
  private cGid = new Float64Array(0); // for hover detail; no per-slice JS objects
  // Interned string columns: keep per-slice memory tiny (Int32 ids) while still
  // resolving the original string on hover. func is low-cardinality; task is one
  // id per request and shared across that request's many spans.
  private cFunc = new Int32Array(0);
  private cTask = new Int32Array(0);
  private cStack = new Int32Array(0);
  private funcTab: string[] = [""];
  private funcMap = new Map<string, number>([["", 0]]);
  private taskTab: string[] = [""];
  private taskMap = new Map<string, number>([["", 0]]);
  private stackTab: string[] = [""];
  private stackMap = new Map<string, number>([["", 0]]);

  // gid -> track id, name
  private trackOf = new Map<number, number>();
  nTracks = 0;
  private trackName: string[] = [];
  private hubTrack: boolean[] = []; // track id -> is it a Hub (waiting, not CPU)
  private trackRun: number[] = []; // track id -> total run-time (ms) = activity
  sortMode: SortMode = "recent1";
  private lastSortMs = 0;
  private placed = 0; // tracks already assigned a row (stable while paused)
  private forceSort = false; // a sort-mode change forces one resort even if paused
  // Time-axis display mode + wall-clock epoch (ms) at trace t0 (NaN if unknown).
  timeMode: "relative" | "current" | "utc" = "relative";
  epochMs = NaN;
  private t0ns = NaN;

  // Incremental CPU histogram: non-Hub run-time (ms) per BIN_MS bin, indexed by
  // floor(timeMs / BIN_MS). Built as slices arrive so the synced graph reads
  // a bounded number of bins per frame instead of rescanning every span.
  private cpuBins = new Float32Array(4096);
  private nBins = 0;

  // GC pauses (global stalls). Infrequent, so plain arrays are fine. start/dur
  // are raw ns (relative to trace t0); converted to the view axis at draw time.
  private gcStart: number[] = [];
  private gcDur: number[] = [];
  private gcGen: number[] = [];
  private gcColl: number[] = [];

  // track id <-> display row, uploaded as a 1×N R32F lookup texture.
  private rowTex: WebGLTexture;
  private rowOf = new Float32Array(0);
  private rowToTrack = new Int32Array(0);
  private rowsDirty = false;

  // View state (ms relative to trace start).
  viewT0 = 0;
  pxPerMs = 1;
  scrollY = 0;
  trackH = 18;
  follow = true;
  private fitted = false;
  // Smooth-follow: the live edge is driven by the LOCAL clock (anchorData maps
  // to anchorLocal = performance.now()), so scrolling is perfectly smooth and
  // independent of when WebSocket batches arrive. A tiny per-frame drift term
  // realigns it with the newest data without visible lurches.
  private anchorData = 0;
  private anchorLocal = 0;
  private wasFollowing = false;

  private mouseX = -1;
  private mouseY = -1;
  private selecting = false; // drag-select a time range to zoom
  private selStartX = 0;
  dragMode: "pan" | "zoom" = "zoom"; // what a plain left-drag on the body does
  private hovered: Hover | null = null;
  onHover: (h: Hover | null) => void = () => {};
  onSelect: (h: Hover | null) => void = () => {};

  constructor(
    private canvas: HTMLCanvasElement,
    private overlay: HTMLCanvasElement,
  ) {
    const gl = canvas.getContext("webgl2", { antialias: false });
    if (!gl) throw new Error("WebGL2 unavailable in this browser");
    this.gl = gl;
    this.ctx = overlay.getContext("2d")!;

    this.prog = gl.createProgram()!;
    gl.attachShader(this.prog, compile(gl, gl.VERTEX_SHADER, VERT));
    gl.attachShader(this.prog, compile(gl, gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(this.prog);
    if (!gl.getProgramParameter(this.prog, gl.LINK_STATUS)) {
      throw new Error("link: " + gl.getProgramInfoLog(this.prog));
    }
    for (const n of [
      "u_res",
      "u_viewT0",
      "u_pxPerMs",
      "u_trackH",
      "u_scrollY",
      "u_rulerH",
      "u_minPx",
      "u_texW",
      "u_rowTex",
    ]) {
      this.u[n] = gl.getUniformLocation(this.prog, n);
    }
    gl.useProgram(this.prog);
    gl.uniform1i(this.u.u_rowTex!, 0);

    this.vao = gl.createVertexArray()!;
    gl.bindVertexArray(this.vao);

    const corners = new Float32Array([0, 0, 1, 0, 0, 1, 0, 1, 1, 0, 1, 1]);
    const bCorner = gl.createBuffer()!;
    gl.bindBuffer(gl.ARRAY_BUFFER, bCorner);
    gl.bufferData(gl.ARRAY_BUFFER, corners, gl.STATIC_DRAW);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);

    this.aStart = { buf: gl.createBuffer()!, loc: 1, size: 1 };
    this.aDur = { buf: gl.createBuffer()!, loc: 2, size: 1 };
    this.aTrack = { buf: gl.createBuffer()!, loc: 3, size: 1 };
    this.aColor = { buf: gl.createBuffer()!, loc: 4, size: 3 };
    this.aSlow = { buf: gl.createBuffer()!, loc: 5, size: 1 };
    this.grow(INIT_CAP);

    this.rowTex = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, this.rowTex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);

    gl.bindVertexArray(null);
    // Shared (dark) background so the track area matches the ruler/CPU bands.
    gl.clearColor(13 / 255, 15 / 255, 19 / 255, 1);
    this.installInput();
    requestAnimationFrame(this.frame);
  }

  /** Grow CPU mirrors + GPU buffers to hold >= minCap instances. GPU data is
   *  copied device-to-device (copyBufferSubData), avoiding CPU→GPU re-uploads. */
  private grow(minCap: number) {
    const gl = this.gl;
    let cap = Math.max(this.cap, INIT_CAP);
    while (cap < minCap) cap *= 2;

    const s = new Float32Array(cap),
      d = new Float32Array(cap),
      t = new Float32Array(cap);
    const c = new Float32Array(cap * 3),
      g = new Float64Array(cap),
      sl = new Float32Array(cap);
    const fn = new Int32Array(cap),
      tk = new Int32Array(cap),
      st = new Int32Array(cap);
    s.set(this.cStart.subarray(0, this.count));
    d.set(this.cDur.subarray(0, this.count));
    t.set(this.cTrack.subarray(0, this.count));
    c.set(this.cColor.subarray(0, this.count * 3));
    g.set(this.cGid.subarray(0, this.count));
    sl.set(this.cSlow.subarray(0, this.count));
    fn.set(this.cFunc.subarray(0, this.count));
    tk.set(this.cTask.subarray(0, this.count));
    st.set(this.cStack.subarray(0, this.count));
    this.cStart = s;
    this.cDur = d;
    this.cTrack = t;
    this.cColor = c;
    this.cGid = g;
    this.cSlow = sl;
    this.cFunc = fn;
    this.cTask = tk;
    this.cStack = st;

    gl.bindVertexArray(this.vao);
    for (const a of [
      this.aStart,
      this.aDur,
      this.aTrack,
      this.aColor,
      this.aSlow,
    ]) {
      const nb = gl.createBuffer()!;
      gl.bindBuffer(gl.ARRAY_BUFFER, nb);
      gl.bufferData(gl.ARRAY_BUFFER, cap * a.size * 4, gl.DYNAMIC_DRAW);
      if (this.count > 0) {
        gl.bindBuffer(gl.COPY_READ_BUFFER, a.buf);
        gl.copyBufferSubData(
          gl.COPY_READ_BUFFER,
          gl.ARRAY_BUFFER,
          0,
          0,
          this.count * a.size * 4,
        );
      }
      gl.deleteBuffer(a.buf);
      a.buf = nb;
      gl.vertexAttribPointer(a.loc, a.size, gl.FLOAT, false, 0, 0);
      gl.enableVertexAttribArray(a.loc);
      gl.vertexAttribDivisor(a.loc, 1);
    }
    this.cap = cap;
  }

  addSlices(slices: Slice[]) {
    if (slices.length === 0) return;
    if (isNaN(this.t0ns)) this.t0ns = slices[0].start;
    if (this.count + slices.length > this.cap)
      this.grow(this.count + slices.length);

    const first = this.count;
    for (const sl of slices) {
      let track = this.trackOf.get(sl.gid);
      if (track === undefined) {
        track = this.nTracks++;
        this.trackOf.set(sl.gid, track);
        this.trackName[track] = sl.name || `0x${sl.gid.toString(16)}`;
        this.hubTrack[track] = /^hub/i.test(sl.name);
        this.trackRun[track] = 0;
        this.rowsDirty = true;
      }
      const i = this.count++;
      this.cStart[i] = (sl.start - this.t0ns) / 1e6;
      this.cDur[i] = sl.dur / 1e6;
      this.cTrack[i] = track;
      this.cGid[i] = sl.gid;
      this.cFunc[i] = this.intern(sl.func, this.funcTab, this.funcMap);
      this.cTask[i] = this.intern(sl.task, this.taskTab, this.taskMap);
      this.cStack[i] = this.intern(sl.stack, this.stackTab, this.stackMap);
      // Highlight slow spans (yellow > 20ms, red > 50ms) — but the Hub waiting
      // in the event loop is not work, so it's never flagged.
      const durMs = sl.dur / 1e6;
      this.cSlow[i] = this.hubTrack[track]
        ? 0
        : durMs > SLOW_MS
          ? 2
          : durMs > WARN_MS
            ? 1
            : 0;
      if (this.cSlow[i] >= 1) this.slowIdx.push(i);
      const endMs = this.cStart[i] + durMs;
      if (endMs > this.spanMs) this.spanMs = endMs;
      this.trackRun[track] += durMs;
      if (!this.hubTrack[track]) this.addCpu(this.cStart[i], durMs);
      const [r, g, b] = this.colorOf(track);
      this.cColor[i * 3] = r;
      this.cColor[i * 3 + 1] = g;
      this.cColor[i * 3 + 2] = b;
    }

    const gl = this.gl;
    const n = this.count - first;
    gl.bindVertexArray(this.vao);
    this.subUpload(this.aStart, this.cStart, first, n);
    this.subUpload(this.aDur, this.cDur, first, n);
    this.subUpload(this.aTrack, this.cTrack, first, n);
    this.subUpload(this.aColor, this.cColor, first, n);
    this.subUpload(this.aSlow, this.cSlow, first, n);
    // recomputeRows is deferred to the frame loop so a burst of new greenlets
    // triggers one resort, not one per greenlet (was O(n²)).
  }

  private subUpload(a: Attr, arr: Float32Array, first: number, n: number) {
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, a.buf);
    gl.bufferSubData(
      gl.ARRAY_BUFFER,
      first * a.size * 4,
      arr.subarray(first * a.size, (first + n) * a.size),
    );
  }

  private recomputeRows() {
    this.rowsDirty = false;
    if (this.rowOf.length < this.nTracks) {
      const ro = new Float32Array(this.nTracks);
      ro.set(this.rowOf);
      const rt = new Int32Array(this.nTracks);
      rt.set(this.rowToTrack);
      this.rowOf = ro;
      this.rowToTrack = rt;
    }

    let order: number[];
    if (!this.follow && !this.forceSort) {
      // Paused: never reorder existing rows; just append any new greenlets at
      // the bottom so the layout stays stable while you inspect.
      order = new Array(this.nTracks);
      for (let r = 0; r < this.placed; r++) order[r] = this.rowToTrack[r];
      let r = this.placed;
      for (let id = this.placed; id < this.nTracks; id++) order[r++] = id;
    } else {
      // Per-track activity for the sort: lifetime total, or run-time within a
      // recent window (so ranking reflects what's busy now).
      let act: number[] | Float64Array | null = null;
      if (this.sortMode === "activity") {
        act = this.trackRun;
      } else if (this.sortMode !== "ident") {
        const win =
          this.sortMode === "recent1"
            ? 1_000
            : this.sortMode === "recent10"
              ? 10_000
              : 60_000;
        const a = new Float64Array(this.nTracks);
        const now = this.maxT();
        const from = now - win;
        for (let i = this.count - 1; i >= 0; i--) {
          const e = this.cStart[i] + this.cDur[i];
          if (e < from) break;
          a[this.cTrack[i]] +=
            Math.min(e, now) - Math.max(this.cStart[i], from);
        }
        act = a;
      }
      order = Array.from({ length: this.nTracks }, (_, i) => i);
      order.sort((a, b) => {
        const ha = this.hubTrack[a],
          hb = this.hubTrack[b];
        if (ha !== hb) return ha ? -1 : 1; // Hub(s) pinned on top
        if (act && !ha) {
          const d = (act[b] || 0) - (act[a] || 0);
          if (d) return d;
        }
        const ka = trackRank(this.trackName[a]);
        const kb = trackRank(this.trackName[b]);
        return (
          ka[0] - kb[0] ||
          ka[1] - kb[1] ||
          (ka[2] < kb[2] ? -1 : ka[2] > kb[2] ? 1 : 0)
        );
      });
    }
    this.forceSort = false;
    this.placed = this.nTracks;
    order.forEach((track, row) => {
      this.rowOf[track] = row;
      this.rowToTrack[row] = track;
    });
    // Upload row map as a 2D R32F texture (TEX_W wide), padded to full rows so
    // it scales past the ~16k single-row texture-width limit.
    const rows = Math.max(1, Math.ceil(this.nTracks / TEX_W));
    const padded = new Float32Array(TEX_W * rows);
    padded.set(this.rowOf.subarray(0, this.nTracks));
    const gl = this.gl;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.rowTex);
    gl.texImage2D(
      gl.TEXTURE_2D,
      0,
      gl.R32F,
      TEX_W,
      rows,
      0,
      gl.RED,
      gl.FLOAT,
      padded,
    );
  }

  reset() {
    this.count = 0;
    this.spanMs = 0;
    this.nTracks = 0;
    this.trackOf.clear();
    this.trackName = [];
    this.hubTrack = [];
    this.trackRun = [];
    this.t0ns = NaN;
    this.fitted = false;
    this.rowsDirty = false;
    this.placed = 0;
    this.forceSort = false;
    this.slowIdx = [];
    this.funcTab = [""];
    this.funcMap = new Map([["", 0]]);
    this.taskTab = [""];
    this.taskMap = new Map([["", 0]]);
    this.stackTab = [""];
    this.stackMap = new Map([["", 0]]);
    this.cpuBins = new Float32Array(4096);
    this.nBins = 0;
    this.gcStart = [];
    this.gcDur = [];
    this.gcGen = [];
    this.gcColl = [];
  }

  /** Add GC pause events (global stalls). */
  addGc(
    events: { start: number; dur: number; gen: number; collected: number }[],
  ) {
    if (events.length === 0) return;
    if (isNaN(this.t0ns)) this.t0ns = events[0].start;
    for (const e of events) {
      this.gcStart.push(e.start);
      this.gcDur.push(e.dur);
      this.gcGen.push(e.gen);
      this.gcColl.push(e.collected);
    }
  }

  gcCount() {
    return this.gcStart.length;
  }

  /** Mean events (greenlet switches) per second over the captured span. */
  eventsPerSec(): number {
    return this.spanMs > 0 ? this.count / (this.spanMs / 1000) : 0;
  }

  /** Captured wall-clock span, in milliseconds. */
  spanMillis(): number {
    return this.spanMs;
  }

  /** Index of the GC pause whose band the cursor x falls in, or -1. */
  private gcAt(px: number): number {
    if (isNaN(this.t0ns)) return -1;
    for (let i = this.gcStart.length - 1; i >= 0; i--) {
      const sMs = (this.gcStart[i] - this.t0ns) / 1e6;
      const x0 = (sMs - this.viewT0) * this.pxPerMs;
      const w = Math.max((this.gcDur[i] / 1e6) * this.pxPerMs, 2);
      if (px >= x0 - 2 && px <= x0 + w + 2) return i;
    }
    return -1;
  }

  /** Accumulate a non-Hub run-interval [startMs, startMs+durMs] into the CPU
   *  histogram, splitting across BIN_MS bins it spans. */
  private addCpu(startMs: number, durMs: number) {
    const e = startMs + durMs;
    let b0 = Math.floor(startMs / BIN_MS);
    const b1 = Math.floor(e / BIN_MS);
    if (b1 >= this.cpuBins.length) {
      let cap = this.cpuBins.length;
      while (b1 >= cap) cap *= 2;
      const nb = new Float32Array(cap);
      nb.set(this.cpuBins.subarray(0, this.nBins));
      this.cpuBins = nb;
    }
    if (b0 < 0) b0 = 0;
    for (let b = b0; b <= b1; b++) {
      const bs = b * BIN_MS;
      this.cpuBins[b] += Math.min(e, bs + BIN_MS) - Math.max(startMs, bs);
    }
    if (b1 + 1 > this.nBins) this.nBins = b1 + 1;
  }

  private intern(s: string, tab: string[], map: Map<string, number>): number {
    if (!s) return 0;
    let id = map.get(s);
    if (id === undefined) {
      id = tab.length;
      tab.push(s);
      map.set(s, id);
    }
    return id;
  }

  /** Lane color: the Hub is always the reserved green; everything else gets a
   *  non-red, non-green hue. */
  private colorOf(track: number): [number, number, number] {
    return this.hubTrack[track] ? HUB_COLOR : trackColor(track);
  }

  private maxT(): number {
    if (this.count === 0) return 0;
    const i = this.count - 1;
    return this.cStart[i] + this.cDur[i];
  }

  /** Max vertical scroll (px) so the last track row can reach the viewport. */
  private maxScrollY(): number {
    const areaH = this.canvas.clientHeight - HEADER_H;
    return Math.max(0, this.nTracks * this.trackH - areaH);
  }

  /** Fraction [0,1] of the recent window spent running non-Hub greenlets — a
   *  CPU-busy proxy for this single-OS-thread process (Hub time = waiting). */
  cpuBusy(windowMs = 1000): number {
    const now = this.maxT();
    if (now <= 0) return 0;
    const from = now - windowMs;
    let busy = 0;
    for (let i = this.count - 1; i >= 0; i--) {
      const s = this.cStart[i];
      const e = s + this.cDur[i];
      if (e < from) break;
      if (this.hubTrack[this.cTrack[i]]) continue;
      busy += Math.min(e, now) - Math.max(s, from);
    }
    return Math.max(0, Math.min(1, busy / windowMs));
  }

  fit() {
    const span = this.maxT();
    if (span <= 0) return;
    const w = this.canvas.clientWidth || 1000;
    this.pxPerMs = Math.min(
      MAX_PXPERMS,
      Math.max(MIN_PXPERMS, (w * 0.96) / span),
    );
    // Keep follow as-is (fit must not cancel it); only reposition when paused.
    if (!this.follow) this.viewT0 = 0;
  }

  setSortMode(m: SortMode) {
    this.sortMode = m;
    this.rowsDirty = true;
    this.forceSort = true; // explicit user action: resort even if paused
    this.lastSortMs = 0;
  }

  setTimeMode(m: "relative" | "current" | "utc") {
    this.timeMode = m;
  }

  setDragMode(m: "pan" | "zoom") {
    this.dragMode = m;
  }

  /** Format a trace-relative time (ms) per the current mode — relative duration,
   *  or local/UTC clock time if the wall-clock epoch is known. */
  private formatAxis(tMs: number): string {
    if (this.timeMode === "relative" || !Number.isFinite(this.epochMs)) {
      return formatTime(tMs);
    }
    const d = new Date(this.epochMs + tMs);
    const p = (n: number, l = 2) => String(n).padStart(l, "0");
    return this.timeMode === "utc"
      ? `${p(d.getUTCHours())}:${p(d.getUTCMinutes())}:${p(d.getUTCSeconds())}.${p(d.getUTCMilliseconds(), 3)}`
      : `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
  }

  /** Zoom limits, for a UI slider. */
  static readonly MIN_ZOOM = MIN_PXPERMS;
  static readonly MAX_ZOOM = MAX_PXPERMS;

  /** Set absolute zoom (px per ms), keeping the viewport center fixed. */
  zoomTo(pxPerMs: number) {
    const cw = this.canvas.clientWidth || 1000;
    const centerT = this.viewT0 + cw / 2 / this.pxPerMs;
    this.pxPerMs = Math.min(MAX_PXPERMS, Math.max(MIN_PXPERMS, pxPerMs));
    this.viewT0 = Math.max(0, centerT - cw / 2 / this.pxPerMs);
  }

  private frame = () => {
    const gl = this.gl;
    const dpr = window.devicePixelRatio || 1;
    const cw = this.canvas.clientWidth;
    const ch = this.canvas.clientHeight;
    const w = Math.floor(cw * dpr);
    const h = Math.floor(ch * dpr);
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
      this.overlay.width = w;
      this.overlay.height = h;
    }
    // Use the ACTUAL backing-to-CSS ratio, not dpr: floor() above can make the
    // backing not an exact dpr multiple (fractional dpr / browser zoom), and the
    // browser then rescales the canvas — which would drift the drawn lanes/CPU
    // band away from the pixel-exact DOM track labels. These keep them aligned.
    const sx = w / cw;
    const sy = h / ch;

    if (!this.fitted && this.count > 0) {
      this.fitted = true;
      // Show ~5s initially (slower, more context) rather than ~1.5s.
      this.pxPerMs = Math.min(MAX_PXPERMS, Math.max(0.01, cw / 5000));
    }

    const nowWall = performance.now();
    if (this.follow) {
      const m = this.maxT();
      if (!this.wasFollowing) {
        this.anchorData = m;
        this.anchorLocal = nowWall;
      }
      // Edge position is purely a function of elapsed LOCAL time → smooth.
      let edge = this.anchorData + (nowWall - this.anchorLocal);
      const drift = m - edge;
      if (Math.abs(drift) > 2000) {
        // startup / tab-resume / big stall: snap.
        this.anchorData = m;
        this.anchorLocal = nowWall;
        edge = m;
      } else {
        // gentle continuous realignment toward the newest data.
        this.anchorData += drift * 0.01;
        edge += drift * 0.01;
      }
      const visMs = cw / this.pxPerMs;
      this.viewT0 = Math.max(0, edge - visMs * 0.9);
    }
    this.wasFollowing = this.follow;

    // In any activity-based mode, re-rank periodically (throttled) — but only
    // while following; when paused, rows stay put so you can inspect.
    if (
      this.sortMode !== "ident" &&
      this.follow &&
      nowWall - this.lastSortMs > 2000
    ) {
      this.rowsDirty = true;
      this.lastSortMs = nowWall;
    }
    // Batched once-per-frame resort of track rows, then clamp vertical scroll.
    if (this.rowsDirty) this.recomputeRows();
    this.scrollY = Math.min(this.scrollY, this.maxScrollY());
    if (this.scrollY < 0) this.scrollY = 0;

    gl.viewport(0, 0, w, h);
    gl.clear(gl.COLOR_BUFFER_BIT);
    if (this.count > 0) {
      gl.useProgram(this.prog);
      gl.bindVertexArray(this.vao);
      gl.uniform2f(this.u.u_res!, w, h);
      gl.uniform1f(this.u.u_viewT0!, this.viewT0);
      gl.uniform1f(this.u.u_pxPerMs!, this.pxPerMs * sx);
      gl.uniform1f(this.u.u_trackH!, this.trackH * sy);
      gl.uniform1f(this.u.u_scrollY!, this.scrollY * sy);
      gl.uniform1f(this.u.u_rulerH!, HEADER_H * sy); // tracks start below ruler + CPU band
      gl.uniform1f(this.u.u_minPx!, MIN_PX * sx);
      gl.uniform1f(this.u.u_texW!, TEX_W);
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, this.rowTex);
      gl.drawArraysInstanced(gl.TRIANGLES, 0, 6, this.count);
    }

    this.drawOverlay(cw, ch, sx, sy);
    requestAnimationFrame(this.frame);
  };

  // Full-width CPU-busy area graph, time-aligned with the spans below (same
  // viewT0/pxPerMs), read from the prebuilt bin histogram so cost is ~O(width).
  // The metric is the single gevent OS-thread's busy fraction (non-Hub run
  // time); 100% = that thread is CPU-bound. Not machine-wide / multi-core.
  private drawCpu(cw: number) {
    const ctx = this.ctx;
    const top = RULER_H;
    // The band is [top, top+CPU_H]; 0% is inset from the bottom and there's a
    // GAP_H gap before the first track, so the 0% line/label never overlaps the
    // Hub lane. 100% is inset from the top for the header row.
    const plotTop = top + 22;
    const plotBot = top + CPU_H - 8;
    const plotH = plotBot - plotTop;
    const yOf = (f: number) => plotBot - Math.min(1, Math.max(0, f)) * plotH;

    ctx.fillStyle = "rgb(13,15,19)"; // same as track background
    ctx.fillRect(0, top, cw, CPU_H);

    // Per-pixel busy fraction = busy run-time landing in that pixel's time span
    // ÷ the pixel's wall span (true utilization / duty cycle), computed exactly
    // by distributing each bin's busy-ms across the pixels it covers. Empty bins
    // are skipped, so an idle service stays cheap. When zoomed out (a pixel is a
    // slice of time) a moving average smooths the inherent per-pixel spikiness;
    // zoomed in, values are exact so a real burst still reads its true 100%.
    const col = new Float32Array(cw);
    const visMs = cw / this.pxPerMs;
    const firstBin = Math.max(0, Math.floor(this.viewT0 / BIN_MS));
    const lastBin = Math.min(
      this.nBins - 1,
      Math.floor((this.viewT0 + visMs) / BIN_MS),
    );
    if (lastBin >= firstBin) {
      const binPx = BIN_MS * this.pxPerMs;
      const busy = new Float32Array(cw); // busy-ms accumulated per pixel column
      for (let b = firstBin; b <= lastBin; b++) {
        const cb = this.cpuBins[b];
        if (cb <= 0) continue;
        const xa = (b * BIN_MS - this.viewT0) * this.pxPerMs;
        const xb = xa + binPx;
        let x = Math.floor(xa);
        if (x < 0) x = 0;
        const xe = Math.min(cw, Math.ceil(xb));
        for (; x < xe; x++) {
          const ov = Math.min(xb, x + 1) - Math.max(xa, x);
          if (ov > 0) busy[x] += cb * (ov / binPx); // busy-ms landing in this px
        }
      }
      for (let x = 0; x < cw; x++) col[x] = Math.min(1, busy[x] * this.pxPerMs);

      if (binPx < 1) {
        const w = Math.max(1, Math.round(cw / 100)); // ±w px moving average
        const pref = new Float32Array(cw + 1);
        for (let x = 0; x < cw; x++) pref[x + 1] = pref[x] + col[x];
        for (let x = 0; x < cw; x++) {
          const a = Math.max(0, x - w);
          const e = Math.min(cw, x + w + 1);
          col[x] = (pref[e] - pref[a]) / (e - a);
        }
      }
    }

    // Guide lines (behind the area).
    ctx.strokeStyle = "rgba(120,130,150,0.12)";
    for (const f of [0, 0.5, 1]) {
      const y = yOf(f);
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(cw, y);
      ctx.stroke();
    }

    // Area + line.
    const base = yOf(0);
    ctx.beginPath();
    ctx.moveTo(0, base);
    for (let x = 0; x < cw; x++) ctx.lineTo(x, yOf(col[x]));
    ctx.lineTo(cw, base);
    ctx.closePath();
    ctx.fillStyle = "rgba(232,181,99,0.18)";
    ctx.fill();
    ctx.beginPath();
    for (let x = 0; x < cw; x++) {
      const y = yOf(col[x]);
      if (x === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.strokeStyle = "#e8b563";
    ctx.lineWidth = 1;
    ctx.stroke();

    // Scale labels in a left gutter, drawn on top with chips so they stay
    // readable over the area.
    ctx.font = "10px ui-monospace, Menlo, monospace";
    ctx.textBaseline = "middle";
    for (const f of [0, 0.5, 1]) {
      const y = yOf(f);
      const t = `${f * 100}%`;
      ctx.fillStyle = "rgba(13,15,19,0.85)";
      ctx.fillRect(AXIS_X - 4, y - 6, 32, 12);
      ctx.fillStyle = "#6b7280";
      ctx.fillText(t, AXIS_X, y);
    }

    // Header row: label on the left, current % on the right (no overlap with
    // the 100% guide, which is below this row).
    const hy = top + 9;
    ctx.fillStyle = "#8b93a3";
    ctx.fillText("CPU · gevent thread (1 core)", AXIS_X, hy);
    const pct = Math.round(this.cpuBusy(1000) * 100);
    ctx.font = "600 12px ui-monospace, Menlo, monospace";
    ctx.fillStyle = pct > 85 ? "#bf616a" : "#e8b563";
    ctx.textAlign = "right";
    ctx.fillText(`${pct}%`, cw - 6, hy);
    ctx.textAlign = "left";

    // Hover readout: detailed % (and time) at the cursor while over the band.
    if (this.mouseX >= 0 && this.mouseY >= top && this.mouseY < top + CPU_H) {
      const x = Math.max(0, Math.min(cw - 1, Math.round(this.mouseX)));
      const v = col[x];
      const y = yOf(v);
      ctx.fillStyle = "#ebcb8b";
      ctx.beginPath();
      ctx.arc(x, y, 3, 0, Math.PI * 2);
      ctx.fill();

      const t = this.viewT0 + this.mouseX / this.pxPerMs;
      const label = `${(v * 100).toFixed(1)}%  ·  ${this.formatAxis(t)}`;
      ctx.font = AXIS_FONT;
      const tw = ctx.measureText(label).width;
      let bx = x + 10;
      if (bx + tw + 8 > cw) bx = x - tw - 14;
      const by = Math.max(top + 19, y - 16);
      ctx.fillStyle = "rgba(33,37,46,0.97)";
      ctx.fillRect(bx - 4, by - 9, tw + 8, 18);
      ctx.strokeStyle = "#3b4252";
      ctx.strokeRect(bx - 4, by - 9, tw + 8, 18);
      ctx.fillStyle = "#ebcb8b";
      ctx.fillText(label, bx, by);
    }
  }

  private drawOverlay(cw: number, ch: number, sx: number, sy: number) {
    const ctx = this.ctx;
    ctx.setTransform(sx, 0, 0, sy, 0, 0);
    ctx.clearRect(0, 0, cw, ch);

    ctx.fillStyle = "rgb(13,15,19)";
    ctx.fillRect(0, 0, cw, RULER_H);

    this.drawCpu(cw);

    // Subtle dividers between ruler / CPU band / track area.
    ctx.strokeStyle = "rgba(255,255,255,0.06)";
    for (const y of [RULER_H, RULER_H + CPU_H, HEADER_H]) {
      ctx.beginPath();
      ctx.moveTo(0, y + 0.5);
      ctx.lineTo(cw, y + 0.5);
      ctx.stroke();
    }

    // Dynamic grid: a "nice" major step (…10/20/50/100/200/500ms…) that adapts
    // to zoom, with 5 faint minor subdivisions between majors.
    const step = niceStep(85 / this.pxPerMs);
    const minor = step / 5;
    ctx.font = AXIS_FONT;
    ctx.textBaseline = "middle";

    // minor lines (batched into one path)
    ctx.strokeStyle = "rgba(120,130,150,0.07)";
    ctx.beginPath();
    for (let t = Math.ceil(this.viewT0 / minor) * minor; ; t += minor) {
      const x = (t - this.viewT0) * this.pxPerMs;
      if (x > cw) break;
      if (x < 0) continue;
      ctx.moveTo(x + 0.5, RULER_H);
      ctx.lineTo(x + 0.5, ch);
    }
    ctx.stroke();

    // major lines + labels
    ctx.strokeStyle = "rgba(130,140,160,0.22)";
    for (let t = Math.ceil(this.viewT0 / step) * step; ; t += step) {
      const x = (t - this.viewT0) * this.pxPerMs;
      if (x > cw) break;
      ctx.beginPath();
      ctx.moveTo(x + 0.5, RULER_H);
      ctx.lineTo(x + 0.5, ch);
      ctx.stroke();
      ctx.fillStyle = "#9aa3b2";
      ctx.fillText(this.formatAxis(t), x + 4, RULER_H / 2);
    }

    // GC pauses: global vertical lines spanning the full height (a GC stalls the
    // whole gevent thread, so it blocks every lane at once). Hover shows
    // gen/duration/objects in the top readout.
    if (!isNaN(this.t0ns)) {
      for (let i = 0; i < this.gcStart.length; i++) {
        const sMs = (this.gcStart[i] - this.t0ns) / 1e6;
        const x0 = (sMs - this.viewT0) * this.pxPerMs;
        // Min 2px so even sub-pixel pauses show as a consistent translucent band.
        const w = Math.max((this.gcDur[i] / 1e6) * this.pxPerMs, 2);
        if (x0 + w < 0 || x0 > cw) continue;
        ctx.fillStyle = "rgba(170,130,210,0.3)";
        ctx.fillRect(x0, RULER_H, w, ch - RULER_H);
      }
    }

    if (this.mouseX >= 0 && this.mouseY >= RULER_H) {
      const t = this.viewT0 + this.mouseX / this.pxPerMs;
      ctx.strokeStyle = "rgba(235,203,139,0.5)";
      ctx.beginPath();
      ctx.moveTo(this.mouseX + 0.5, RULER_H);
      ctx.lineTo(this.mouseX + 0.5, ch);
      ctx.stroke();

      let label = this.formatAxis(t);
      const gi = this.gcAt(this.mouseX);
      if (gi >= 0) {
        label += `   ·   GC gen${this.gcGen[gi]} ${formatTime(this.gcDur[gi] / 1e6)} (${this.gcColl[gi]} freed)`;
      } else if (this.hovered) {
        label += `   ·   dur ${formatTime(this.hovered.durNs / 1e6)}`;
      }
      const tw = ctx.measureText(label).width;
      const bx = Math.min(this.mouseX + 8, cw - tw - 10);
      ctx.fillStyle = "rgba(33,37,46,0.95)";
      ctx.fillRect(bx - 4, 2, tw + 8, RULER_H - 4);
      ctx.fillStyle = "#ebcb8b";
      ctx.fillText(label, bx, RULER_H / 2);
    }

    // Drag-select marquee (zoom to range on release).
    if (this.selecting && this.mouseX >= 0) {
      const xa = Math.min(this.selStartX, this.mouseX);
      const xb = Math.max(this.selStartX, this.mouseX);
      ctx.fillStyle = "rgba(136,192,208,0.18)";
      ctx.fillRect(xa, RULER_H, xb - xa, ch - RULER_H);
      ctx.strokeStyle = "rgba(136,192,208,0.8)";
      ctx.strokeRect(xa + 0.5, RULER_H + 0.5, xb - xa, ch - RULER_H - 1);
    }

    // Vertical scrollbar when the greenlet list overflows the track area.
    const areaH = ch - HEADER_H;
    const total = this.nTracks * this.trackH;
    if (total > areaH) {
      const sbW = 5,
        sbX = cw - sbW - 1;
      ctx.fillStyle = "rgba(255,255,255,0.05)";
      ctx.fillRect(sbX, HEADER_H, sbW, areaH);
      const thumbH = Math.max(24, (areaH * areaH) / total);
      const denom = total - areaH;
      const thumbY =
        HEADER_H + (denom > 0 ? this.scrollY / denom : 0) * (areaH - thumbH);
      ctx.fillStyle = "rgba(180,190,210,0.38)";
      ctx.fillRect(sbX, thumbY, sbW, thumbH);
    }
  }

  private installInput() {
    const cv = this.canvas;
    cv.addEventListener(
      "wheel",
      (e) => {
        e.preventDefault();
        if (e.ctrlKey || e.metaKey) {
          // zoom time around cursor — does NOT cancel follow (stays anchored live)
          const tAtCursor = this.viewT0 + e.offsetX / this.pxPerMs;
          const factor = Math.exp(-e.deltaY * ZOOM_SENS);
          this.pxPerMs = Math.min(
            MAX_PXPERMS,
            Math.max(MIN_PXPERMS, this.pxPerMs * factor),
          );
          this.viewT0 = tAtCursor - e.offsetX / this.pxPerMs;
        } else if (e.shiftKey) {
          this.viewT0 += e.deltaY / this.pxPerMs; // pan time
          this.follow = false;
        } else {
          // scroll the greenlet list vertically (keeps follow on); only a
          // horizontal-DOMINANT wheel pans time and cancels follow.
          const dy =
            e.deltaMode === 1 ? e.deltaY * this.trackH * 3 : e.deltaY * 14;
          this.scrollY = Math.max(
            0,
            Math.min(this.maxScrollY(), this.scrollY + dy),
          );
          if (Math.abs(e.deltaX) > Math.abs(e.deltaY)) {
            this.viewT0 += e.deltaX / this.pxPerMs;
            this.follow = false;
          }
        }
      },
      { passive: false },
    );

    let dragging = false,
      lastX = 0,
      lastY = 0,
      downX = 0,
      downY = 0,
      moved = false;
    cv.addEventListener("mousedown", (e) => {
      moved = false;
      lastX = downX = e.offsetX;
      lastY = downY = e.offsetY;
      // Select-a-range-to-zoom when: in zoom drag-mode, Shift held, or dragging
      // on the time ruler. Otherwise a plain drag pans.
      if (this.dragMode === "zoom" || e.shiftKey || e.offsetY < RULER_H) {
        this.selecting = true;
        this.selStartX = e.offsetX;
        this.setHover(null);
      } else {
        dragging = true;
      }
    });
    window.addEventListener("mouseup", () => {
      if (this.selecting) {
        this.selecting = false;
        if (moved)
          this.zoomToRange(this.selStartX, this.mouseX); // drag = zoom
        else this.onSelect(this.pickAt(downX, downY)); // click still selects span
      } else if (dragging && !moved) {
        this.onSelect(this.pickAt(downX, downY)); // a click selects the span
      }
      dragging = false;
    });
    cv.addEventListener("mousemove", (e) => {
      this.mouseX = e.offsetX;
      this.mouseY = e.offsetY;
      if (Math.abs(e.offsetX - downX) > 3 || Math.abs(e.offsetY - downY) > 3)
        moved = true;
      if (this.selecting) {
        return; // marquee is drawn from selStartX..mouseX in the frame loop
      }
      if (dragging) {
        this.viewT0 -= (e.offsetX - lastX) / this.pxPerMs;
        this.scrollY = Math.max(0, this.scrollY - (e.offsetY - lastY));
        lastX = e.offsetX;
        lastY = e.offsetY;
        // only a horizontal drag (time pan) cancels follow; vertical = list scroll
        if (Math.abs(e.offsetX - downX) > 3) this.follow = false;
        this.setHover(null);
      } else {
        this.setHover(this.pickAt(e.offsetX, e.offsetY));
      }
    });
    cv.addEventListener("mouseleave", () => {
      this.mouseX = this.mouseY = -1;
      this.setHover(null);
    });
    cv.addEventListener("dblclick", () => this.fit());
  }

  /** Zoom so the screen-x range [a, b] fills the viewport. */
  private zoomToRange(a: number, b: number) {
    const xa = Math.min(a, b);
    const xb = Math.max(a, b);
    const tA = this.viewT0 + xa / this.pxPerMs;
    const tB = this.viewT0 + xb / this.pxPerMs;
    const spanMs = Math.max(tB - tA, 1e-6);
    const w = this.canvas.clientWidth || 1000;
    this.pxPerMs = Math.min(MAX_PXPERMS, Math.max(MIN_PXPERMS, w / spanMs));
    this.viewT0 = Math.max(0, tA);
    this.follow = false;
  }

  private setHover(h: Hover | null) {
    this.hovered = h;
    this.onHover(h);
  }

  /** The span (with detail) at a screen position, or null. */
  private hoverFromIndex(i: number, px: number, py: number): Hover {
    const track = this.cTrack[i];
    return {
      name: this.trackName[track],
      gid: this.cGid[i],
      startNs: this.cStart[i] * 1e6 + this.t0ns,
      durNs: this.cDur[i] * 1e6,
      func: this.funcTab[this.cFunc[i]],
      task: this.taskTab[this.cTask[i]],
      stack: this.stackTab[this.cStack[i]],
      x: px,
      y: py,
    };
  }

  private pickAt(px: number, py: number): Hover | null {
    const t = this.viewT0 + px / this.pxPerMs;
    const row = Math.floor((py - HEADER_H + this.scrollY) / this.trackH);
    if (row < 0 || row >= this.nTracks) return null;
    const track = this.rowToTrack[row];
    const slopMs = MIN_PX / this.pxPerMs;
    for (let i = this.count - 1; i >= 0; i--) {
      if (this.cTrack[i] !== track) continue;
      const s = this.cStart[i];
      if (t >= s && t <= s + Math.max(this.cDur[i], slopMs)) {
        return this.hoverFromIndex(i, px, py);
      }
    }
    return null;
  }

  slowCount() {
    return this.slowIdx.length;
  }

  /** Recent warn/slow spans (newest first) for the slow-log panel. */
  slowSpans(limit = 500): {
    idx: number;
    startNs: number;
    durNs: number;
    name: string;
    level: number;
    func: string;
    gid: number;
  }[] {
    const out = [];
    for (let k = this.slowIdx.length - 1; k >= 0 && out.length < limit; k--) {
      const i = this.slowIdx[k];
      out.push({
        idx: i,
        startNs: this.cStart[i] * 1e6 + this.t0ns,
        durNs: this.cDur[i] * 1e6,
        name: this.trackName[this.cTrack[i]],
        level: this.cSlow[i],
        func: this.funcTab[this.cFunc[i]],
        gid: this.cGid[i],
      });
    }
    return out;
  }

  /** Jump the view to a span (center it, zoom to frame it, select it). */
  revealSpan(idx: number) {
    if (idx < 0 || idx >= this.count) return;
    const sMs = this.cStart[idx];
    const durMs = this.cDur[idx];
    const w = this.canvas.clientWidth || 1000;
    this.follow = false;
    this.pxPerMs = Math.min(
      MAX_PXPERMS,
      Math.max(MIN_PXPERMS, (w * 0.3) / Math.max(durMs, 0.001)),
    );
    this.viewT0 = Math.max(0, sMs + durMs / 2 - w / this.pxPerMs / 2);
    const row = this.rowOf[this.cTrack[idx]] ?? 0;
    const areaH = this.canvas.clientHeight - HEADER_H;
    this.scrollY = Math.max(
      0,
      Math.min(this.maxScrollY(), row * this.trackH - areaH / 2),
    );
    this.onSelect(this.hoverFromIndex(idx, 0, 0));
  }

  /** Labels for the currently *visible* rows only — so the DOM stays small no
   *  matter how many greenlets exist. Each carries its lane color and total
   *  run-time (activity) for the y-axis. */
  trackLabels(): { name: string; y: number; color: string; runMs: number }[] {
    const areaH = this.canvas.clientHeight - HEADER_H;
    const firstRow = Math.max(0, Math.floor(this.scrollY / this.trackH));
    const lastRow = Math.min(
      this.nTracks - 1,
      Math.ceil((this.scrollY + areaH) / this.trackH),
    );
    const out: { name: string; y: number; color: string; runMs: number }[] = [];
    for (let row = firstRow; row <= lastRow; row++) {
      const track = this.rowToTrack[row];
      if (track === undefined || this.trackName[track] === undefined) continue;
      const [r, g, b] = this.colorOf(track);
      out.push({
        name: this.trackName[track],
        y: HEADER_H + row * this.trackH - this.scrollY,
        color: `rgb(${(r * 255) | 0},${(g * 255) | 0},${(b * 255) | 0})`,
        runMs: this.trackRun[track] || 0,
      });
    }
    return out;
  }

  rowHeight() {
    return this.trackH;
  }

  headerHeight() {
    return HEADER_H;
  }
}
