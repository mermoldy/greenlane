// WebGL2 instanced-quad timeline renderer + 2D axis/grid overlay.
//
// One greenlet run-interval = one instanced rectangle whose WIDTH is its real
// duration (dur * pxPerMs). Pan/zoom are uniform changes only — geometry is
// never rebuilt on the CPU per frame. New executions append with bufferSubData; the
// buffers are preallocated large and grow (rarely) via GPU-to-GPU copies, so
// there are no periodic CPU→GPU re-upload stalls.
//
// Track display order is decoupled from insert order via a track→row lookup
// texture (Hub on top, greenlets by ident), so reordering never touches the
// per-execution buffers.
//
// Known v1 simplifications (deliberate seams for later):
//  - Times are f32 milliseconds relative to trace start (~µs precision over
//    minutes). For long captures, rebase the origin or use a server LOD query.
//  - Picking is a linear scan (fine to ~1e5 visible). Per-track index later.
//  - No LOD yet: zoomed all the way out, sub-pixel executions hit the 1px floor.

export interface Execution {
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

const MIN_PX = 1; // smallest rendered execution width; proportional above this
// How far before the trace start (ms) the view may scroll — a small buffer for
// zoom-out breathing room, but not infinite empty space.
const LEFT_BUFFER_MS = 120_000;
const MAX_PXPERMS = 1000; // max zoom = 1px per 1µs
const MIN_PXPERMS = 1 / 200; // min zoom = 1px per 200ms
const ZOOM_SENS = 0.0008; // wheel-zoom sensitivity (lower = finer/slower)
const RULER_H = 22; // top time-ruler band, CSS px
const CPU_H = 60; // CPU graph band under the ruler, CSS px
const LAG_H = 30; // kernel scheduler-lag band under the CPU band (R13), CSS px
const GAP_H = 7; // gap between the lag band and the first track
const HEADER_H = RULER_H + CPU_H + LAG_H + GAP_H; // tracks begin below this
// The lag band auto-scales its y-axis to the tallest sample in the visible window
// (a fixed ceiling either clamps real spikes or squashes quiet stretches — 10 and
// 500 ms/s ended up at the same height). This is the *floor* for that full-scale
// value: until lag exceeds it, the band tops out here so routine sub-ms jitter
// stays low instead of being amplified to fill the band.
const LAG_MIN_FULL_MS_S = 50;
const AXIS_X = 6; // shared left inset for axis labels (CPU % and track names)

// Hover explanations for the "?" badges on the CPU and scheduler-lag band headers.
const CPU_HELP =
  "Busy fraction of the single gevent OS thread: non-Hub greenlet run time ÷ " +
  "wall time. 100% = that one thread is CPU-bound. gevent runs on one core, so " +
  "this is not machine-wide CPU. Hover for the value at a point in time.";
const LAG_HELP =
  "Kernel run-queue delay for the hub thread: milliseconds it was runnable but " +
  "NOT on a CPU, per wall-second (from /proc schedstat). It's a rate out of " +
  "1000 ms/s — e.g. 200 ms/s means 20% of wall-time was spent waiting for a core. " +
  "Rough scale: <5 = healthy, 5–50 = some contention, ≥50 worth investigating — " +
  "severe starvation (oversubscription / cgroup·k8s CFS throttling) climbs from " +
  "there. The band's y-axis auto-scales to the tallest lag in view (its full-scale " +
  "value is shown at the band top), so heights are relative to the current window. " +
  "High lag means a long execution here was off-CPU waiting to run, not your code " +
  "being slow. Linux-only.";
const INIT_CAP = 1 << 14; // ~16k executions preallocated (cheap doubling grows after)
// When a full window load leaves the buffer this many times larger than what's
// actually held, shrink it back down so resident memory tracks the working set
// rather than the high-water mark of a one-off zoomed-out window. The 4× slack is
// hysteresis so small zoom wiggles don't thrash the realloc.
const SHRINK_SLACK = 4;
// While following live, the viewer appends just the new tail each tick instead of
// re-fetching the whole window. Past this many buffered rows it does one full
// (re-centered) window load instead, bounding memory + keeping the f32 GPU-relative
// `start` base near the view. Matches the server's WINDOW_CAP.
const APPEND_MAX_ROWS = 2_000_000;
const WARN_MS = 20; // executions longer than this get a yellow border
const SLOW_MS = 50; // executions longer than this get a red border
const BIN_MS = 1; // CPU histogram resolution: non-Hub run-time per 1ms bin
const TEX_W = 4096; // row-lookup texture width; 2D-wrapped to scale past 16k tracks
// Max distinct greenlets a *capped live* session keeps before folding further new
// gids into one shared "(other)" greenlet. The server already evicts old row DATA in
// these sessions; this bounds the per-greenlet metadata (names, colors, row map) so a
// long run that churns through millions of short-lived tasks can't grow it
// without limit. Generous — a timeline with this many greenlets is already unreadable;
// the cap is a memory guard, not a display choice. Recordings are never capped.
const MAX_LIVE_TRACKS = 20_000;
// Shared axis font — matches the DOM track labels for consistent styling.
const AXIS_FONT = "11px ui-monospace, 'SF Mono', Menlo, monospace";

/** Adaptive, **compact** time formatting (a few significant digits) for the axis
 *  ruler and at-a-glance labels. Input is ms. For exact execution durations/timestamps
 *  use [`formatTimePrecise`]. */
export function formatTime(ms: number): string {
  const a = Math.abs(ms);
  if (a < 1e-3) return `${(ms * 1e6).toFixed(0)} ns`;
  if (a < 1) return `${(ms * 1e3).toFixed(a < 0.1 ? 1 : 0)} µs`;
  if (a < 1e3) return `${ms.toFixed(a < 10 ? 2 : 1)} ms`;
  return `${(ms / 1e3).toFixed(2)} s`;
}

/** Full-precision time, down to the nanosecond, with trailing zeros trimmed — for
 *  execution durations and start/end readouts where exactness matters (the compact
 *  [`formatTime`] rounds to a few digits). Input is ms. Examples: `1234567` ns →
 *  `1.234567 ms`; `1_000_000` ns → `1 ms`; `1_234_567_890` ns → `1.23456789 s`. */
export function formatTimePrecise(ms: number): string {
  const ns = Math.round(ms * 1e6);
  const a = Math.abs(ns);
  // Trim trailing zeros (and a now-bare decimal point) from a fixed-decimal string.
  const trim = (s: string) => s.replace(/\.?0+$/, "");
  if (a < 1_000) return `${ns} ns`;
  if (a < 1_000_000) return `${trim((ns / 1e3).toFixed(3))} µs`;
  if (a < 1_000_000_000) return `${trim((ns / 1e6).toFixed(6))} ms`;
  return `${trim((ns / 1e9).toFixed(9))} s`;
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
layout(location=1) in float a_start;   // ms relative to the loaded window's t0
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
    // v_slow: 1 = warn (>20ms, yellow), 2 = slow (>50ms, red). Colors mirror the
    // CSS tokens --ac-block (#e8606b) and --ac-warn (#ebcb8b).
    float dx = min(v_local.x, 1.0 - v_local.x) * v_sizePx.x;
    float dy = min(v_local.y, 1.0 - v_local.y) * v_sizePx.y;
    if (min(dx, dy) < 2.0) {
      col = v_slow > 1.5 ? vec3(0.91, 0.376, 0.42) : vec3(0.922, 0.796, 0.545);
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
// executions are red — so greenlet hues are mapped to bands that avoid both.
const HUB_COLOR: [number, number, number] = [163 / 255, 190 / 255, 140 / 255];

// "duration" color mode: fill greenlet executions by how long they ran rather than by
// identity — blue under the warn threshold (ok), yellow up to the block
// threshold, red beyond it. The Hub keeps its theme green. Yellow/red match the
// highlight-border tones so the two modes read consistently.
const OK_COLOR: [number, number, number] = [0.36, 0.62, 0.92]; // blue: < warn
// Warn/block fills mirror the CSS tokens --ac-warn (#ebcb8b) and --ac-block
// (#e8606b); loadTheme() overrides these from the live tokens at construction.
const DUR_WARN_COLOR: [number, number, number] = [0.922, 0.796, 0.545]; // #ebcb8b
const DUR_SLOW_COLOR: [number, number, number] = [0.91, 0.376, 0.42]; // #e8606b

/** "#rrggbb" → 0..255 RGB triple (for ctx rgba() strings). */
function hexToRgb255(hex: string): [number, number, number] {
  const n = parseInt(hex.trim().replace("#", ""), 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
/** "#rrggbb" → 0..1 RGB triple (for WebGL fills). */
function hexToRgb01(hex: string): [number, number, number] {
  const [r, g, b] = hexToRgb255(hex);
  return [r / 255, g / 255, b / 255];
}

// "heatmap" color mode: a continuous inferno-style gradient (cool→hot) by run
// length, so duration reads as intensity rather than the three discrete bands of
// "duration" mode. Stops climb dark-indigo → purple → magenta → orange → yellow:
// perceptually monotonic, bright enough to stay legible on the dark track, and it
// avoids green (the Hub's reserved color). Position is log-scaled (see heatRgb).
const HEAT_STOPS: Array<[number, [number, number, number]]> = [
  [0.0, [0.15, 0.16, 0.45]], // dark indigo — coolest / shortest
  [0.3, [0.4, 0.18, 0.58]], // purple
  [0.55, [0.74, 0.21, 0.45]], // magenta-red
  [0.78, [0.93, 0.45, 0.18]], // orange
  [1.0, [0.98, 0.86, 0.32]], // yellow — hottest / longest
];
function heatGradient(t: number): [number, number, number] {
  const x = t <= 0 ? 0 : t >= 1 ? 1 : t;
  for (let i = 1; i < HEAT_STOPS.length; i++) {
    const [p1, c1] = HEAT_STOPS[i];
    if (x <= p1) {
      const [p0, c0] = HEAT_STOPS[i - 1];
      const f = (x - p0) / (p1 - p0);
      return [
        c0[0] + (c1[0] - c0[0]) * f,
        c0[1] + (c1[1] - c0[1]) * f,
        c0[2] + (c1[2] - c0[2]) * f,
      ];
    }
  }
  return HEAT_STOPS[HEAT_STOPS.length - 1][1];
}

// Stable greenlet color keyed off the greenlet NAME (not insertion index), so a
// greenlet keeps its color even as windows are swapped in and out of memory.
//
// To stay distinct across *thousands* of greenlets we spread them over hue AND
// saturation AND lightness (one hue band alone only yields a few dozen telling
// colors). The driving index is the greenlet's numeric id when the name carries
// one (e.g. "Greenlet-1234") so consecutive greenlets land far apart; otherwise
// a hash of the name. Each channel advances by a distinct irrational step (a
// low-discrepancy / golden-ratio sequence) so the whole palette fills evenly
// without clustering. Hue stays in allowed bands — orange/yellow [25,82) and
// cyan/blue/purple/magenta [162,336) — skipping red (slow executions) and green (Hub).
function colorForName(name: string): [number, number, number] {
  const m = name.match(/(\d+)(?!.*\d)/); // last run of digits, if any
  let idx: number;
  if (m) {
    idx = parseInt(m[1], 10);
  } else {
    let h = 2166136261 >>> 0; // FNV-1a
    for (let i = 0; i < name.length; i++) {
      h ^= name.charCodeAt(i);
      h = Math.imul(h, 16777619) >>> 0;
    }
    idx = h;
  }
  const frac = (n: number, step: number) => (n * step) % 1;
  // Hue: golden-angle spread, remapped into the two allowed bands.
  const seg1 = 57,
    seg2 = 174; // band widths
  const pos = frac(idx, 0.6180339887) * (seg1 + seg2);
  const hue = pos < seg1 ? 25 + pos : 162 + (pos - seg1);
  // Saturation + lightness vary too (each its own irrational step) for a much
  // larger distinct palette; kept in ranges that read well on the dark theme.
  const sat = 0.48 + 0.34 * frac(idx, 0.7548776662);
  const light = 0.5 + 0.22 * frac(idx, 0.569840291);
  return hsl(hue, sat, light);
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

// How execution fill is colored: "ident" = a stable per-greenlet color (default);
// "duration" = blue/yellow/red by run length (Hub stays green); "heatmap" = a
// continuous cool→hot gradient by run length (log-scaled; Hub stays green).
export type ColorMode = "ident" | "duration" | "heatmap";

export class Timeline {
  private gl: WebGL2RenderingContext;
  private ctx: CanvasRenderingContext2D;
  // 2D-overlay colors, read once from the CSS custom-property tokens (:root in
  // styles.css) by loadTheme(), so the canvas and DOM chrome share one palette.
  // Defaults match the tokens in case they can't be resolved.
  private tBg = "#0d0f13"; // --bg-app (band/track background)
  private tMuted = "#8b93a3"; // --tx-muted (band labels)
  private tText2 = "#cdd3de"; // --tx-2 (help "?" text)
  private tWarn = "#ebcb8b"; // --ac-warn
  private tBlock = "#e8606b"; // --ac-block
  private tCpu = "#e8b563"; // --ac-cpu (CPU line)
  private tGc = "#c8a0e6"; // --ac-gc (GC band)
  private hubRgb = HUB_COLOR; // --ac-green, as WebGL float triple
  private durWarnRgb = DUR_WARN_COLOR;
  private durSlowRgb = DUR_SLOW_COLOR;
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
  private cGid = new Float64Array(0); // execution identity for hover (→ lazy detail fetch)

  // gid -> track id, name
  private trackOf = new Map<number, number>();
  nTracks = 0;
  private trackName: string[] = [];
  private hubTrack: boolean[] = []; // track id -> is it a Hub (waiting, not CPU)
  private trackRun: number[] = []; // track id -> total run-time (ms) = activity
  // track id -> longest span (ms) seen. Drives the mode-aware row swatch: in
  // duration/heatmap mode the greenlet's label dot shows its worst span's color.
  private trackMaxDur: number[] = [];
  // Whether the live session caps how much it keeps in memory (server retention
  // horizon). When set, greenlet identity is bounded too: see overflowTrack().
  retentionActive = false;
  // Shared "(other greenlets)" track that bounds greenlet-identity growth once a capped
  // live session exceeds MAX_LIVE_TRACKS distinct gids. -1 until first needed.
  private overflowRt = -1;
  sortMode: SortMode = "recent1";
  private lastSortMs = 0;
  // Greenlet rows are kept STABLE across window swaps: once a greenlet has a row it
  // keeps it; new greenlets append at the bottom; a full re-sort happens only on
  // an explicit sort-mode change or (while following) a throttled interval. This
  // is what keeps the Hub pinned, `ident` order steady, and the selected greenlet
  // from scrolling off when zooming changes which greenlets are in the window.
  private placed = 0; // greenlets already assigned a stable row
  private forceSort = false; // a sort-mode change forces one full re-sort
  // Time-axis display mode + wall-clock epoch (ms) at trace t0 (NaN if unknown).
  timeMode: "relative" | "current" | "utc" = "relative";
  epochMs = NaN;
  private t0ns = NaN;

  // Incremental CPU histogram: non-Hub run-time (ms) per BIN_MS bin, indexed by
  // floor(timeMs / BIN_MS). Built as executions arrive so the synced graph reads
  // a bounded number of bins per frame instead of rescanning every execution.
  private cpuBins = new Float32Array(4096);
  private nBins = 0;
  // Reused per-frame scratch for the CPU/lag band column math, so following/
  // animating doesn't allocate fresh Float32Array(cw)s every frame (GC churn).
  // Reallocated only when the canvas widens; callers clear the prefix they use.
  private scrCpuCol = new Float32Array(0);
  private scrCpuBusy = new Float32Array(0);
  private scrCpuPref = new Float32Array(0);
  private scrLagCol = new Float32Array(0);
  private fitScr = (
    a: Float32Array<ArrayBuffer>,
    len: number,
  ): Float32Array<ArrayBuffer> => (a.length >= len ? a : new Float32Array(len));

  // GC pauses (global stalls). Infrequent, so plain arrays are fine. start/dur
  // are raw ns (relative to trace t0); converted to the view axis at draw time.
  private gcStart: number[] = [];
  private gcDur: number[] = [];
  private gcGen: number[] = [];
  private gcColl: number[] = [];
  // Kernel scheduler-lag samples (R13): the hub thread's run-queue-wait rate
  // (ms of starvation per wall-second) at trace time `lagT` (ms since t0). Pushed
  // from each live `head` and plotted at the live edge, so it aligns to the execution
  // axis with no cross-process clock mapping. Bounded ring (oldest dropped).
  private lagT: number[] = [];
  private lagV: number[] = [];
  // Live hub-thread CPU-utilization samples ([0,1] fraction) at trace time `cpuT`
  // (ms since t0), pushed from each live `head`. Like lag, these are an execution-
  // stream-independent /proc measure, so they keep the CPU band's tail moving in the
  // pending area (ahead of the lagging trace data). Bounded ring (oldest dropped).
  private cpuT: number[] = [];
  private cpuV: number[] = [];
  // Whether the GC marker layer is drawn (toggleable from the toolbar). The data
  // is always collected/counted; this only gates rendering + hover readout.
  showGc = true;

  // track id <-> display row, uploaded as a 1×N R32F lookup texture.
  private rowTex: WebGLTexture;
  private rowOf = new Float32Array(0);
  private rowToTrack = new Int32Array(0);
  private rowsDirty = false;

  // View state (ms relative to trace start).
  viewT0 = 0;
  pxPerMs = 1;
  scrollY = 0;
  // ms from origin to the loaded window's t0. The GPU execution buffer (`a_start`) holds
  // window-relative starts, so the render rebases `u_viewT0` by this much; both
  // operands of `a_start - u_viewT0` stay small → no f32 cancellation on long runs.
  private glBaseMs = 0;
  trackH = 18;
  follow = true;
  private fitted = false;
  // Live session? When true, "now" is driven by the wall clock (Date.now-epochMs)
  // so the follow edge advances in real time and the arrival lag is visible; when
  // false (recording) the edge is the end of the captured data.
  live = false;
  // Server flagged the tracer as stalled (live, but executions stopped advancing —
  // the target's switch hook went quiet). Treated like non-live for the edge/lag so
  // the view stops chasing the wall clock; auto-clears when data resumes. Set by the
  // app from `head.stalled`.
  tracerStalled = false;

  // ── Viewport windowing ────────────────────────────────────────────────
  // Only the visible window's executions live in memory; the server is queried as
  // the view changes. A FIXED global origin (ns, from server meta) keeps view
  // coordinates stable across window swaps; `fullSpanMs` is the whole capture's
  // extent (for fit/follow) even though we hold only a window.
  private originNs = 0;
  private originSet = false;
  fullSpanMs = 0;
  // Span highlight thresholds (ms): warn = yellow border, slow/block = red.
  // Defaults match the server; overridden from `meta` (--warn-ms/--block-ms).
  warnMs = WARN_MS;
  slowMs = SLOW_MS;
  // Greenlet fill: by execution duration (default), greenlet identity, or heatmap.
  colorMode: ColorMode = "duration";
  /** Fired (throttled) when the visible range changes → app requests that window.
   *  Args: absolute t0/t1 in ns, canvas width in px, and — when following live —
   *  `append`: the data frontiers (`from` = max start held, `gcFrom` = max GC start
   *  held) to request only the new tail to APPEND. Absent → a full window to replace. */
  onViewport:
    | ((
        t0ns: number,
        t1ns: number,
        px: number,
        append?: { from: number; gcFrom: number },
      ) => void)
    | null = null;
  private lastVpMs = 0;
  // At most one viewport request is outstanding at a time. Following re-requests
  // every ~250ms, but a large (zoomed-out) window's reply can take longer than
  // that to compute + decode; without this guard each reply is superseded by a
  // newer request and dropped as stale, so the timeline freezes and never picks
  // up new data. `windowApplied()` (called when a reply lands) clears it; the
  // VP_TIMEOUT backstop re-arms following if a reply is ever lost.
  private vpInFlight = false;
  private vpSentMs = 0;
  // Diagnostics (read by the UI's debug logging): wall time the last window took
  // to ingest+upload, and whether the server truncated it (hit the execution cap).
  lastLoadMs = 0;
  lastWindowCapped = false;
  // Absolute ns range currently loaded in memory. We fetch a window WIDER than
  // the viewport (a margin on each side), so panning/zooming within it needs no
  // server round-trip — we only refetch when the view leaves this range (or, when
  // following, on a slower timer to pick up new data at the live edge).
  private loadedT0 = 0;
  private loadedT1 = -1;
  // Live-follow data frontiers (absolute ns): the max execution start and max GC
  // start we currently hold. The next tail request asks for data STRICTLY PAST these
  // — not past `loadedT1` (the requested window edge), which while following runs
  // ahead of the data by the arrival lag, so a window-edge cursor would forever ask
  // for an empty future and nothing new would ever render.
  private dataCursorNs = 0;
  private gcCursorNs = 0;
  // Whether the last frame reported the timeline as start-sorted. Append assumes
  // monotonic start arrival; once a capture goes multi-thread (out-of-order spans)
  // this flips false and the viewer falls back to full windows. (See C1.) Set by the
  // app from each window/tail frame's `sorted` flag.
  lastSorted = true;
  // perf-clock of the last append whose tail came back empty — used to back off the
  // fast follow cadence on an idle target (no new spans to fetch). (See R1.)
  private lastEmptyTailMs = -1;
  // A execution highlighted from the slow-log (ms relative to origin + click time, for
  // the flash). Drawn as a bright time band so the clicked execution is easy to find.
  private hl: { t0Ms: number; t1Ms: number; at: number } | null = null;
  // Pending vertical scroll target (greenlet gid) from a slow-log click: the
  // greenlet may not be in memory yet, so we scroll once its window streams in.
  private scrollToGid: number | null = null;

  private mouseX = -1;
  private mouseY = -1;
  // Render-on-demand: the frame loop skips the GL draw + 2D overlay when nothing
  // changed. `dirty` is set by input, data arrival, and discrete display-setting
  // changes; the loop also force-draws while following/selecting/flashing, when the
  // view scalars move, on cursor movement, and ~2×/s as a safety valve so a missed
  // wake can never freeze the UI.
  private dirty = true;
  private mouseMoved = false;
  private idleFrames = 0;
  private lastDrawViewT0 = NaN;
  private lastDrawPx = NaN;
  private lastDrawScrollY = NaN;
  // Hover picking is coalesced to once per frame: mousemove only flags this, and the
  // frame loop runs the (potentially long) pickAt at most once per frame.
  private pickPending = false;
  // Longest buffered execution duration (ms) — lets pickAt stop scanning the
  // time-ordered buffer early (no earlier-starting span can still cover the cursor).
  private maxDurMs = 0;
  // Teardown (see `dispose`): cancel the RAF loop and remove every input listener
  // (registered with `ac.signal`) so a remount — e.g. React StrictMode's double
  // mount — doesn't leave an old renderer + listener set running.
  private rafId = 0;
  private disposed = false;
  private readonly ac = new AbortController();
  // "?" help badges (band headers): rebuilt each frame as they're drawn, then
  // hit-tested against the cursor to show their explanation tooltip.
  private helpRects: { x: number; y: number; r: number; text: string }[] = [];
  private selecting = false; // drag-select a time range to zoom
  private selStartX = 0;
  dragMode: "pan" | "zoom" = "zoom"; // what a plain left-drag on the body does
  private hovered: Hover | null = null;
  // The span whose trace panel (right menu) is open — outlined persistently until
  // the panel closes (see clickSelect / clearSelection), independent of hover.
  private selectedSpan: Hover | null = null;
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
    this.loadTheme();

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
    this.rafId = requestAnimationFrame(this.frame);
  }

  /** Stop the render loop and remove all input listeners. Call from the React
   *  effect cleanup so a remount (incl. StrictMode's dev double-mount) doesn't
   *  leave a second RAF loop + listener set alive on the same canvas/window. */
  dispose() {
    this.disposed = true;
    cancelAnimationFrame(this.rafId);
    this.ac.abort();
  }

  /** Grow CPU mirrors + GPU buffers to hold >= minCap instances (doubling). */
  private grow(minCap: number) {
    let cap = Math.max(this.cap, INIT_CAP);
    while (cap < minCap) cap *= 2;
    if (cap !== this.cap) this.setCap(cap);
  }

  /** After a full window load, release a buffer that has grown far larger than the
   *  data it now holds (e.g. a one-off zoomed-out window), so resident CPU+GPU
   *  memory tracks the working set rather than the session high-water mark. Gated
   *  by SHRINK_SLACK hysteresis; only call on full loads, never appends. */
  private maybeShrink() {
    if (this.cap <= INIT_CAP || this.count * SHRINK_SLACK >= this.cap) return;
    let cap = INIT_CAP;
    while (cap < this.count) cap *= 2;
    if (cap < this.cap) this.setCap(cap);
  }

  /** Reallocate CPU mirrors + GPU buffers to exactly `cap` instances, preserving
   *  the first `count` rows (device-to-device copy on the GPU side). Shared by
   *  grow (up) and maybeShrink (down). */
  private setCap(cap: number) {
    const gl = this.gl;
    const s = new Float32Array(cap),
      d = new Float32Array(cap),
      t = new Float32Array(cap);
    const c = new Float32Array(cap * 3),
      g = new Float64Array(cap),
      sl = new Float32Array(cap);
    s.set(this.cStart.subarray(0, this.count));
    d.set(this.cDur.subarray(0, this.count));
    t.set(this.cTrack.subarray(0, this.count));
    c.set(this.cColor.subarray(0, this.count * 3));
    g.set(this.cGid.subarray(0, this.count));
    sl.set(this.cSlow.subarray(0, this.count));
    this.cStart = s;
    this.cDur = d;
    this.cTrack = t;
    this.cColor = c;
    this.cGid = g;
    this.cSlow = sl;

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

  private subUpload(a: Attr, arr: Float32Array, first: number, n: number) {
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, a.buf);
    gl.bufferSubData(
      gl.ARRAY_BUFFER,
      first * a.size * 4,
      arr.subarray(first * a.size, (first + n) * a.size),
    );
  }

  /** Upload `src[0 .. n*size]` to the buffer starting at instance `gpuFirst` — for
   *  appending a tail whose source array is zero-based (not the full CPU mirror). */
  private subUploadAt(a: Attr, src: Float32Array, gpuFirst: number, n: number) {
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, a.buf);
    gl.bufferSubData(
      gl.ARRAY_BUFFER,
      gpuFirst * a.size * 4,
      src.subarray(0, n * a.size),
    );
  }

  private recomputeRows() {
    this.rowsDirty = false;
    this.dirty = true; // row order/texture changed → repaint
    if (this.rowOf.length < this.nTracks) {
      const ro = new Float32Array(this.nTracks);
      ro.set(this.rowOf);
      const rt = new Int32Array(this.nTracks);
      rt.set(this.rowToTrack);
      this.rowOf = ro;
      this.rowToTrack = rt;
    }

    let order: number[];
    if (!this.follow && !this.forceSort && this.placed > 0) {
      // Established order + not following: keep existing rows stable; only append
      // greenlets newly seen in this window at the bottom. (placed === 0 means no
      // order yet, so fall through to a full sort — which pins the Hub.)
      order = new Array(this.nTracks);
      for (let r = 0; r < this.placed; r++) order[r] = this.rowToTrack[r];
      let r = this.placed;
      for (let id = this.placed; id < this.nTracks; id++) order[r++] = id;
    } else {
      // Full sort: Hub(s) pinned on top, then greenlets by the chosen mode.
      // Per-track activity = lifetime total, or run-time within a recent window.
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
    this.trackMaxDur = [];
    this.overflowRt = -1;
    this.t0ns = this.originSet ? this.originNs : NaN;
    this.fitted = false;
    this.selectedSpan = null;
    this.rowsDirty = false;
    this.placed = 0;
    this.forceSort = false;
    this.cpuBins = new Float32Array(4096);
    this.nBins = 0;
    this.gcStart = [];
    this.gcDur = [];
    this.gcGen = [];
    this.gcColl = [];
    this.lagT = [];
    this.lagV = [];
    this.cpuT = [];
    this.cpuV = [];
    this.maxDurMs = 0;
    // Streaming state: drop the loaded window + live-follow cursors so the next load
    // starts fresh (a full window, never an append against stale data). Needed on a
    // reconnect to a *restarted* server whose origin differs (C2).
    this.loadedT0 = 0;
    this.loadedT1 = -1;
    this.dataCursorNs = 0;
    this.gcCursorNs = 0;
    this.lastEmptyTailMs = -1;
    this.lastWindowCapped = false;
    this.lastSorted = true;
    this.dirty = true;
  }

  /** Add GC pause events (global stalls). */
  addGc(
    events: { start: number; dur: number; gen: number; collected: number }[],
  ) {
    if (events.length === 0) return;
    this.dirty = true;
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
    if (!this.showGc || isNaN(this.t0ns)) return -1;
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

  /** Read the shared palette from the CSS custom-property tokens (:root in
   *  styles.css). Custom props inherit, so the canvas element resolves them; an
   *  unresolved token keeps the matching default. Called once at construction. */
  private loadTheme() {
    const cs = getComputedStyle(this.canvas);
    const v = (name: string, fallback: string) =>
      cs.getPropertyValue(name).trim() || fallback;
    this.tBg = v("--bg-app", this.tBg);
    this.tMuted = v("--tx-muted", this.tMuted);
    this.tText2 = v("--tx-2", this.tText2);
    this.tWarn = v("--ac-warn", this.tWarn);
    this.tBlock = v("--ac-block", this.tBlock);
    this.tCpu = v("--ac-cpu", this.tCpu);
    this.tGc = v("--ac-gc", this.tGc);
    this.hubRgb = hexToRgb01(v("--ac-green", "#a3be8c"));
    this.durWarnRgb = hexToRgb01(this.tWarn);
    this.durSlowRgb = hexToRgb01(this.tBlock);
  }

  /** rgba() string from a token hex + alpha — for translucent 2D overlays. */
  private rgba(hex: string, a: number): string {
    const [r, g, b] = hexToRgb255(hex);
    return `rgba(${r},${g},${b},${a})`;
  }

  /** Greenlet color: the Hub is always the reserved green; everything else gets a
   *  non-red, non-green hue. */
  private colorOf(track: number): [number, number, number] {
    return this.hubTrack[track]
      ? this.hubRgb
      : colorForName(this.trackName[track] ?? "");
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
    // Fit the WHOLE captured span (server-reported), not just the in-memory
    // window — the window then loads to match.
    const span = this.fullSpanMs > 0 ? this.fullSpanMs : this.maxT();
    if (span <= 0) return;
    const w = this.canvas.clientWidth || 1000;
    this.pxPerMs = Math.min(
      MAX_PXPERMS,
      Math.max(MIN_PXPERMS, (w * 0.96) / span),
    );
    this.fitted = true; // an explicit fit suppresses the initial auto-zoom
    // Keep follow as-is (fit must not cancel it); only reposition when paused.
    if (!this.follow) this.viewT0 = 0;
  }

  setSortMode(m: SortMode) {
    this.sortMode = m;
    this.rowsDirty = true;
    this.forceSort = true; // explicit user action: resort even if paused
    this.lastSortMs = 0;
    this.dirty = true;
  }

  setTimeMode(m: "relative" | "current" | "utc") {
    this.timeMode = m;
    this.dirty = true;
  }

  /** Fix the global time origin (ns). All windows are positioned relative to it,
   *  so view coordinates stay stable when the dataset is swapped per viewport. */
  setOrigin(ns: number) {
    this.originNs = ns;
    this.originSet = true;
    this.t0ns = ns;
  }

  /** Whole captured span (ns) — drives fit()/follow even though only a window is
   *  held in memory. */
  setSpan(spanNs: number) {
    this.fullSpanMs = spanNs / 1e6;
    this.dirty = true; // edge / pending-region position changed
  }

  /** Record a kernel scheduler-lag sample (R13) at the current live edge. `rate`
   *  is run-queue-wait ms per wall-second, or null where unsupported (no sample
   *  stored then). Called from each live `head`. */
  addLag(spanNs: number, rate: number | null) {
    if (rate == null || !isFinite(rate)) return;
    this.dirty = true;
    const t = spanNs / 1e6;
    // Coalesce duplicate edges (head can repeat the same execution) by overwriting.
    const n = this.lagT.length;
    if (n && t <= this.lagT[n - 1]) {
      this.lagV[n - 1] = rate;
      return;
    }
    this.lagT.push(t);
    this.lagV.push(rate);
    // Bounded ring: ~1 sample/100ms, so 36k ≈ 1h of history.
    if (this.lagT.length > 36_000) {
      this.lagT.shift();
      this.lagV.shift();
    }
  }

  /** Record a live hub-thread CPU sample at the current live edge. `cpuMsPerSec` is
   *  on-CPU ms per wall-second (server `head`); stored as a [0,1] utilization fraction
   *  and plotted as the CPU band's live tail in the pending area. null → no sample. */
  addCpuSample(spanNs: number, cpuMsPerSec: number | null) {
    if (cpuMsPerSec == null || !isFinite(cpuMsPerSec)) return;
    this.dirty = true;
    const t = spanNs / 1e6;
    const frac = Math.max(0, Math.min(1, cpuMsPerSec / 1000));
    const n = this.cpuT.length;
    if (n && t <= this.cpuT[n - 1]) {
      this.cpuV[n - 1] = frac; // coalesce duplicate edges
      return;
    }
    this.cpuT.push(t);
    this.cpuV.push(frac);
    if (this.cpuT.length > 36_000) {
      this.cpuT.shift();
      this.cpuV.shift();
    }
  }

  /** The live edge in trace-relative ms: "now" (wall clock) for a live session,
   *  else the end of captured data. `max` with the data span guards against clock
   *  skew so real executions are never clipped past the edge. */
  liveEdgeMs(): number {
    // A stalled tracer (server says executions stopped advancing) is treated like a
    // non-live session for the edge: clamp to the data span so the view stops
    // chasing the wall clock into empty space (and stops re-polling for nothing).
    if (this.live && !this.tracerStalled && Number.isFinite(this.epochMs)) {
      return Math.max(Date.now() - this.epochMs, this.fullSpanMs);
    }
    return this.fullSpanMs;
  }

  /** Arrival lag (ms): how far the latest rendered data trails real time — i.e.
   *  "now" minus the newest captured span. 0 for recordings / unknown epoch / a
   *  stalled tracer (no live edge to trail). */
  liveLagMs(): number {
    if (this.live && !this.tracerStalled && Number.isFinite(this.epochMs)) {
      return Math.max(0, Date.now() - this.epochMs - this.fullSpanMs);
    }
    return 0;
  }

  /** Warn/block execution-duration thresholds (ms), from the server config, so execution
   *  highlight colors match the slow-log filter. */
  setThresholds(warnMs: number, slowMs: number) {
    this.warnMs = warnMs;
    this.slowMs = slowMs;
    this.dirty = true;
  }

  /** Show or hide the GC marker layer. The continuous render loop picks the
   *  change up next frame; GC data stays collected and counted regardless. */
  setShowGc(v: boolean) {
    this.showGc = v;
    this.dirty = true;
  }

  /** Greenlet fill mode: "ident" (per-greenlet color), "duration" (blue/yellow/red
   *  bands by run length), or "heatmap" (continuous cool→hot gradient by run
   *  length). The Hub keeps its theme green in every mode. */
  setColorMode(m: ColorMode) {
    if (this.colorMode === m) return;
    this.colorMode = m;
    // Recolor every execution already in memory and re-upload its color buffer; the
    // render loop is continuous, so the change appears on the next frame.
    for (let i = 0; i < this.count; i++) {
      const [r, g, b] = this.fillRgb(this.cTrack[i], this.cDur[i]);
      this.cColor[i * 3] = r;
      this.cColor[i * 3 + 1] = g;
      this.cColor[i * 3 + 2] = b;
    }
    if (this.count > 0) {
      this.gl.bindVertexArray(this.vao);
      this.subUpload(this.aColor, this.cColor, 0, this.count);
    }
    this.dirty = true;
  }

  /** Fill color for one execution under the current color mode. The Hub is always
   *  its reserved green; non-Hub greenlets get identity / duration-band / heatmap
   *  color depending on the mode. Single source of truth for both the live load
   *  path and setColorMode's recolor. */
  private fillRgb(track: number, durMs: number): [number, number, number] {
    if (!this.hubTrack[track]) {
      if (this.colorMode === "duration") return this.durRgb(durMs);
      if (this.colorMode === "heatmap") return this.heatRgb(durMs);
    }
    return this.colorOf(track);
  }

  /** Duration-mode RGB for a execution of `durMs` (non-Hub). */
  private durRgb(durMs: number): [number, number, number] {
    return durMs >= this.slowMs
      ? this.durSlowRgb
      : durMs >= this.warnMs
        ? this.durWarnRgb
        : OK_COLOR;
  }

  /** Heatmap-mode RGB for a execution of `durMs` (non-Hub): log-scaled position
   *  from a sub-millisecond floor up to the block threshold, mapped onto the
   *  cool→hot gradient. Log scale so the common short runs still spread across the
   *  cool end instead of all collapsing to one color. */
  private heatRgb(durMs: number): [number, number, number] {
    const floor = 0.5; // ms — at/below this is the coolest color
    const top = Math.max(this.slowMs, this.warnMs * 2, floor * 4); // hottest at/above
    const t =
      (Math.log(Math.max(durMs, floor)) - Math.log(floor)) /
      (Math.log(top) - Math.log(floor));
    return heatGradient(t);
  }

  /** Record the absolute ns range a freshly-loaded window covers, so the frame
   *  loop knows when a pan/zoom stays inside it and can skip a refetch. Call AFTER
   *  loadWindowColumnar.
   *
   *  When the window was `capped`, the server truncated it to the contiguous
   *  *center* of the requested range, so the only range we can trust we hold is
   *  exactly the data we received: `[minStartNs, maxEndNs]`. Beyond that — on
   *  either truncated edge — executions may be missing, so a pan there must refetch.
   *  When not capped we got everything in the requested window (empty margins are
   *  genuinely empty), so the full requested range is trustworthy. */
  setLoadedRange(
    t0ns: number,
    t1ns: number,
    minStartNs: number,
    maxEndNs: number,
    capped: boolean,
    n: number,
  ) {
    this.lastWindowCapped = capped;
    if (capped && n > 0) {
      this.loadedT0 = minStartNs;
      this.loadedT1 = maxEndNs;
    } else {
      this.loadedT0 = t0ns;
      this.loadedT1 = t1ns;
    }
  }

  /** A viewport reply has arrived (or was dropped) — clear the in-flight guard so
   *  follow can issue the next request. Bounds outstanding requests to one. */
  windowApplied() {
    this.vpInFlight = false;
  }

  /** Whether the next live-follow request can be a cheap tail APPEND rather than a
   *  full window. All of these must hold; otherwise we re-load a full window:
   *   - following a live session, with a non-empty, uncapped window already loaded;
   *   - the timeline is start-sorted (C1: multi-thread → out-of-order spans would be
   *     skipped by the start-frontier cursor, so those must full-load);
   *   - under the row budget (bounds memory + keeps the f32 GPU base near the view);
   *   - the visible left edge is still within what we hold (appends only extend the
   *     right edge — a zoom-out past loadedT0 needs a re-widened full window). */
  private canAppendNow(visT0: number): boolean {
    return (
      this.follow &&
      this.live &&
      !this.tracerStalled &&
      this.lastSorted &&
      this.count > 0 &&
      !this.lastWindowCapped &&
      this.count < APPEND_MAX_ROWS &&
      Number.isFinite(this.glBaseMs) &&
      this.loadedT1 > this.loadedT0 &&
      visT0 >= this.loadedT0
    );
  }

  /** The shared "(other greenlets)" track, created on first use. Bounds greenlet-identity
   *  growth in a capped live session (see MAX_LIVE_TRACKS). */
  private overflowTrack(): number {
    if (this.overflowRt < 0) {
      this.overflowRt = this.nTracks++;
      this.trackName[this.overflowRt] = "…(other greenlets)";
      this.hubTrack[this.overflowRt] = false;
      this.trackRun[this.overflowRt] = 0;
      this.rowsDirty = true;
    }
    return this.overflowRt;
  }

  /** Append `n` rows from a binary columnar frame into the buffers starting at
   *  instance index `off`, mapping window tracks → persistent renderer tracks and
   *  extending every per-window accumulator (trackRun, CPU bins, span, maxDur). The
   *  single source of truth for turning a frame into GPU rows — shared by the full
   *  load (`off` 0, after a reset) and the live-follow append (`off` = count) so the
   *  two paths can't drift. `baseMs` is this frame's origin-relative base; each row is
   *  rebased to the buffer's fixed `glBaseMs` before upload, keeping the shader's
   *  coordinate space stable across appends (a full load sets `glBaseMs == baseMs`, so
   *  the rebase is a no-op and `rel == startMs`). Per-execution detail
   *  (func/task/stack) isn't shipped — it's fetched lazily on hover. */
  private appendRows(
    off: number,
    baseMs: number,
    tracks: { gid: number; name: string; isHub: boolean; runNs: number }[],
    gc: { start: number; dur: number; gen: number; collected: number }[],
    startMs: Float32Array,
    durMs: Float32Array,
    trackIdx: Uint32Array,
  ) {
    const n = startMs.length;
    // Map each window track to a (persistent) renderer track by gid, creating new
    // greenlets for ones not seen before (folding into the shared overflow track past
    // the live cap). Keeps gid → track stable so rows don't reshuffle across frames.
    const trackMap = new Array<number>(tracks.length);
    for (let ti = 0; ti < tracks.length; ti++) {
      const t = tracks[ti];
      let rt = this.trackOf.get(t.gid);
      if (rt === undefined) {
        if (this.retentionActive && this.nTracks >= MAX_LIVE_TRACKS) {
          rt = this.overflowTrack();
        } else {
          rt = this.nTracks++;
          this.trackOf.set(t.gid, rt);
          this.trackName[rt] = t.name || `0x${t.gid.toString(16)}`;
          this.hubTrack[rt] = t.isHub;
          this.rowsDirty = true;
        }
      }
      if (this.trackRun[rt] === undefined) this.trackRun[rt] = 0;
      trackMap[ti] = rt;
    }

    if (n > 0) {
      if (off + n > this.cap) this.grow(off + n);
      // Rows arrive relative to this frame's baseMs; rebase to glBaseMs for the GPU.
      const relBase = baseMs - this.glBaseMs;
      const rel = new Float32Array(n); // GPU-relative starts for this batch
      for (let i = 0; i < n; i++) {
        const rt = trackMap[trackIdx[i]];
        const d = durMs[i];
        const absStart = baseMs + startMs[i]; // absolute ms since origin (f64)
        const j = off + i;
        this.cStart[j] = absStart;
        this.cDur[j] = d;
        this.cTrack[j] = rt;
        this.cGid[j] = tracks[trackIdx[i]].gid;
        this.cSlow[j] = this.hubTrack[rt]
          ? 0
          : d >= this.slowMs
            ? 2
            : d >= this.warnMs
              ? 1
              : 0;
        rel[i] = relBase + startMs[i];
        const endMs = absStart + d;
        if (endMs > this.spanMs) this.spanMs = endMs;
        if (d > this.maxDurMs) this.maxDurMs = d;
        this.trackRun[rt] += d;
        if (d > (this.trackMaxDur[rt] || 0)) this.trackMaxDur[rt] = d;
        if (!this.hubTrack[rt]) this.addCpu(absStart, d);
        const [r, g, b] = this.fillRgb(rt, d);
        this.cColor[j * 3] = r;
        this.cColor[j * 3 + 1] = g;
        this.cColor[j * 3 + 2] = b;
      }
      this.count = off + n;

      const gl = this.gl;
      gl.bindVertexArray(this.vao);
      // aStart gets the rebased relative starts (small → f32-precise); cStart keeps the
      // absolute ms for CPU-side hit-testing/CPU bins. Other columns upload straight
      // from their mirrors at the matching offset.
      this.subUploadAt(this.aStart, rel, off, n);
      this.subUpload(this.aDur, this.cDur, off, n);
      this.subUpload(this.aTrack, this.cTrack, off, n);
      this.subUpload(this.aColor, this.cColor, off, n);
      this.subUpload(this.aSlow, this.cSlow, off, n);
    }
    if (gc.length) this.addGc(gc);
    this.dirty = true; // new data → redraw
  }

  /** Load a freshly-fetched window from the server's **binary columnar frame**,
   *  REPLACING the previous window's executions. Keeps greenlet identity (gid → track,
   *  names, hub-ness, rows) so greenlets don't reshuffle when the window changes; only
   *  the per-window execution/CPU/GC data resets. View state (zoom/pan/follow) and the
   *  fixed origin are preserved. */
  loadWindowColumnar(
    t0Ns: number,
    maxStartNs: number,
    tracks: { gid: number; name: string; isHub: boolean; runNs: number }[],
    gc: { start: number; dur: number; gen: number; collected: number }[],
    startMs: Float32Array,
    durMs: Float32Array,
    trackIdx: Uint32Array,
  ) {
    const _loadStart = performance.now();
    // Reset the live-follow frontiers to this fresh window: the row frontier is the
    // server's exact maxStart; the GC frontier is the latest GC we now hold, floored
    // at t0 so the next tail doesn't re-fetch GC from before this window.
    this.dataCursorNs = maxStartNs;
    let gcMax = t0Ns;
    for (const e of gc) if (e.start > gcMax) gcMax = e.start;
    this.gcCursorNs = gcMax;
    this.lastEmptyTailMs = -1; // fresh data, not an idle tail
    // Reset per-window data; keep greenlet identity for stable rows.
    this.count = 0;
    this.spanMs = 0;
    this.maxDurMs = 0;
    this.trackRun = new Array(this.nTracks).fill(0);
    this.trackMaxDur = new Array(this.nTracks).fill(0);
    this.cpuBins = new Float32Array(4096);
    this.nBins = 0;
    this.gcStart = [];
    this.gcDur = [];
    this.gcGen = [];
    this.gcColl = [];
    // A full load anchors glBaseMs at this window's base, so appendRows' per-row
    // rebase is a no-op (rel == startMs). The shader rebases u_viewT0 by glBaseMs.
    const baseMs = (t0Ns - this.originNs) / 1e6;
    this.glBaseMs = baseMs;
    this.appendRows(0, baseMs, tracks, gc, startMs, durMs, trackIdx);
    // Reclaim a buffer left oversized by an earlier zoomed-out window (full loads
    // only — the append path must never shrink out from under live rows).
    this.maybeShrink();
    this.lastLoadMs = performance.now() - _loadStart;
  }

  /** APPEND a live-follow tail frame (executions with start ≥ the previously-loaded
   *  right edge) to the existing buffer — instead of re-loading the whole window.
   *  This is the fast path while following: each tick ships only the few new rows at
   *  the live edge, so there's no full re-decode/re-upload of the visible window.
   *
   *  Preconditions (enforced by the caller, see the frame loop): we already hold a
   *  window (`count > 0`), it wasn't capped, and the global origin is set. Greenlet
   *  identity, the GPU-relative base (`glBaseMs`), zoom/pan/follow and all per-window
   *  accumulators (trackRun, CPU bins, GC, span) are KEPT and extended — only the new
   *  rows are added. `t0Ns` is the tail frame's base (server encodes `startMs`
   *  relative to it); we rebase each row to the existing `glBaseMs` before upload so
   *  the shader's coordinate space is unchanged. */
  loadWindowAppend(
    t0Ns: number,
    t1Ns: number,
    maxStartNs: number,
    tracks: { gid: number; name: string; isHub: boolean; runNs: number }[],
    gc: { start: number; dur: number; gen: number; collected: number }[],
    startMs: Float32Array,
    durMs: Float32Array,
    trackIdx: Uint32Array,
  ) {
    const _loadStart = performance.now();
    // Advance the frontiers past this strictly-new tail (see Query::Tail): the GC
    // cursor past any GC here, the row cursor to the server's maxStart (0 = empty
    // tail, which can't exceed the cursor → no-op). appendRows then adds the rows at
    // the current end, keeping the buffer and all accumulators intact.
    for (const e of gc)
      if (e.start > this.gcCursorNs) this.gcCursorNs = e.start;
    if (maxStartNs > this.dataCursorNs) this.dataCursorNs = maxStartNs;
    const baseMs = (t0Ns - this.originNs) / 1e6;
    this.appendRows(this.count, baseMs, tracks, gc, startMs, durMs, trackIdx);
    // R1: an empty tail means the target produced no new spans this round (idle) —
    // flag it so the follow cadence backs off; any rows clear the flag (fast again).
    this.lastEmptyTailMs = startMs.length === 0 ? performance.now() : -1;
    // Extend the loaded range's right edge; the left edge (and glBaseMs) are unchanged.
    this.loadedT1 = Math.max(this.loadedT1, t1Ns);
    this.lastLoadMs = performance.now() - _loadStart;
  }

  /** Snapshot of streaming/chunking state for the UI's debug console logging. */
  metrics() {
    const visMs = this.canvas.clientWidth / this.pxPerMs;
    return {
      loadedExecutions: this.count,
      loadedTracks: this.nTracks,
      loadedMs: [
        (this.loadedT0 - this.originNs) / 1e6,
        (this.loadedT1 - this.originNs) / 1e6,
      ] as [number, number],
      viewMs: [this.viewT0, this.viewT0 + visMs] as [number, number],
      fullSpanMs: this.fullSpanMs,
      lagMs: this.liveLagMs(),
      pxPerMs: this.pxPerMs,
      follow: this.follow,
      inFlight: this.vpInFlight,
      lastLoadMs: this.lastLoadMs,
      capped: this.lastWindowCapped,
    };
  }

  /** Center the view on an absolute time (ns), canceling follow. */
  seekTo(ns: number) {
    this.follow = false;
    const cw = this.canvas.clientWidth || 1000;
    const tMs = (ns - this.originNs) / 1e6;
    this.viewT0 = Math.max(0, tMs - cw / this.pxPerMs / 2);
  }

  /** Jump to and zoom in on a execution (absolute ns), framing it at ~40% of the
   *  viewport, scrolling vertically to its greenlet (`gid`), and marking it with a
   *  highlight band. Used by the slow-log click; the window streams in around it,
   *  and the vertical scroll is applied once that greenlet is present. */
  revealSpanAt(startNs: number, durNs: number, gid: number) {
    const cw = this.canvas.clientWidth || 1000;
    const durMs = Math.max(durNs / 1e6, 1e-3);
    const sMs = (startNs - this.originNs) / 1e6;
    this.follow = false;
    this.pxPerMs = Math.min(
      MAX_PXPERMS,
      Math.max(MIN_PXPERMS, (cw * 0.4) / durMs),
    );
    this.viewT0 = Math.max(0, sMs + durMs / 2 - cw / this.pxPerMs / 2);
    this.hl = { t0Ms: sMs, t1Ms: sMs + durMs, at: performance.now() };
    this.scrollToGid = gid;
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
    if (this.disposed) return; // stopped — don't render or re-arm
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
      this.dirty = true; // resized backing store → must repaint
    }
    // Use the ACTUAL backing-to-CSS ratio, not dpr: floor() above can make the
    // backing not an exact dpr multiple (fractional dpr / browser zoom), and the
    // browser then rescales the canvas — which would drift the drawn greenlets/CPU
    // band away from the pixel-exact DOM track labels. These keep them aligned.
    const sx = w / cw;
    const sy = h / ch;

    if (!this.fitted && this.count > 0) {
      this.fitted = true;
      // Show ~5s initially (slower, more context) rather than ~1.5s.
      this.pxPerMs = Math.min(MAX_PXPERMS, Math.max(0.01, cw / 5000));
    }

    const nowWall = performance.now();
    const visMs = cw / this.pxPerMs;
    // Live edge ("now") driven by the wall clock for a live session, or the end
    // of the data for a recording. The view's right edge may never go past it,
    // and the left edge is bounded by a small buffer (no infinite scroll either
    // way). While following, the right edge tracks the live edge via the internal
    // clock, so the timeline scrolls smoothly and executions fill in behind it as they
    // arrive (the gap to the data = the arrival lag).
    const liveEdge = this.liveEdgeMs();
    const minViewT0 = -LEFT_BUFFER_MS;
    const maxViewT0 = Math.max(minViewT0, liveEdge - visMs);
    if (this.follow) {
      this.viewT0 = maxViewT0;
    }
    this.viewT0 = Math.min(maxViewT0, Math.max(minViewT0, this.viewT0));

    // Request a window from the server only when needed — not every frame. The
    // visible range maps to absolute ns; we fetch it expanded by a margin on each
    // side, and skip the request entirely while the view stays inside what's
    // already loaded. That makes pan/zoom within the margin instant (no round
    // trip); following refetches on a slower timer to pick up the live edge.
    if (this.onViewport) {
      const visT0 = this.originNs + Math.max(0, this.viewT0) * 1e6;
      const visT1 = this.originNs + (this.viewT0 + visMs) * 1e6;
      const inside = visT0 >= this.loadedT0 && visT1 <= this.loadedT1;
      // Is the view watching the live present (its right edge near the newest
      // data)? Then keep it refreshing even when paused, so new executions stream in
      // while you hold the view still. Inspecting older (immutable) data → quiet.
      const viewRightMs = this.viewT0 + visMs;
      const nearLive = this.live && viewRightMs >= this.fullSpanMs - visMs;
      // Following: refresh ~4/s normally, but BACK OFF when the last window was
      // expensive to load (large/zoomed-out captures hit the execution cap and each
      // reload is a heavy decode+upload that blocks the main thread). Refetching
      // every 250ms then never finishes — lag balloons with no visual progress.
      // Cap main-thread occupancy at ~1/3 by spacing refetches at ~3× the last
      // load cost (clamped 250ms…2s); a capped window forces at least 1s.
      // Live-follow APPEND fast path: when following a live session and we already
      // hold an uncapped window, fetch only the new tail `[loadedT1, …]` and append
      // it — no full window re-decode/re-upload each tick. Falls back to a full
      // (re-centered) load past APPEND_MAX_ROWS so memory + the f32 GPU base stay
      // bounded, or when the last window was capped (its bounds are untrustworthy).
      const canAppend = this.canAppendNow(visT0);
      // Following: refresh fast when appending (each tail is cheap), else ~4/s and
      // BACK OFF when the last full window was expensive to load (large/zoomed-out
      // captures hit the execution cap and each reload is a heavy decode+upload that
      // blocks the main thread). Refetching every 250ms then never finishes — lag
      // balloons with no visual progress. Cap main-thread occupancy at ~1/3 by
      // spacing full refetches at ~3× the last load cost (clamped 250ms…2s); a
      // capped window forces at least 1s. R1: once a tail came back empty (idle
      // target), back the append cadence off to 500ms so we don't poll 8×/s for
      // nothing — it snaps back to 120ms the moment a tail brings rows.
      const appendIv = this.lastEmptyTailMs >= 0 ? 500 : 120;
      const followIv = Math.max(
        canAppend ? appendIv : this.lastWindowCapped ? 1000 : 250,
        Math.min(2000, this.lastLoadMs * 3),
      );
      const due = this.follow
        ? nowWall - this.lastVpMs >= followIv
        : (!inside && nowWall - this.lastVpMs >= 100) ||
          (nearLive && nowWall - this.lastVpMs >= 700);
      // Only one request in flight: wait for the prior reply before issuing the
      // next. Critical when zoomed out over a big capture — a window can be the
      // 200k-execution cap (~5 MB) and take many seconds to compute + ship (esp. over
      // a port-forward). VP_TIMEOUT is only a backstop for a genuinely *lost*
      // reply, so it MUST exceed a slow-but-working round-trip; too small and it
      // re-fires mid-flight, piling a server-side backlog whose replies all land
      // stale → the timeline freezes and lag grows without bound. Disconnects
      // re-arm immediately via windowApplied() on ws close/meta, so this large
      // value only delays recovery from the rare silent-drop case.
      const VP_TIMEOUT = 30000;
      const free = !this.vpInFlight || nowWall - this.vpSentMs >= VP_TIMEOUT;
      if (due && free) {
        this.lastVpMs = nowWall;
        if (canAppend) {
          // Tail = everything past our DATA frontier (dataCursorNs), up to the live
          // edge + a small forward margin. Crucially the lower bound is the frontier
          // of data we hold, NOT loadedT1 (the requested edge) — while following the
          // latter sits ahead of the data by the arrival lag, so using it would ask
          // for an empty future and nothing new would stream in.
          const margin = (visT1 - visT0) * 0.25 || 1e6;
          const fromNs = Math.floor(this.dataCursorNs);
          const t1ns = Math.ceil(visT1 + margin);
          if (t1ns > fromNs) {
            this.vpInFlight = true;
            this.vpSentMs = nowWall;
            // t0 = fromNs so the server encodes startMs relative to the frontier
            // (small → f32-precise); the append object carries the row/GC frontiers.
            this.onViewport(fromNs, t1ns, Math.max(1, Math.ceil(cw)), {
              from: fromNs,
              gcFrom: Math.floor(this.gcCursorNs),
            });
          }
        } else {
          this.vpInFlight = true;
          this.vpSentMs = nowWall;
          // Margin = a viewport width on each side (loaded window ≈ 3× visible) so
          // panning/zooming within it needs no round-trip. But once a window is
          // already hitting the execution cap, that 3× just triples an
          // already-too-big, slow fetch for prefetch we'll likely never pan into at
          // this zoom — so shrink the margin hard while capped (fetch ≈ the visible
          // range).
          const marginFrac = this.lastWindowCapped ? 0.1 : 1;
          const margin = (visT1 - visT0) * marginFrac || 1e6;
          const t0ns = Math.max(0, Math.floor(visT0 - margin));
          const t1ns = Math.ceil(visT1 + margin);
          this.onViewport(t0ns, t1ns, Math.max(1, Math.ceil(cw)));
        }
      }
    }

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
    // Apply a pending slow-log scroll once the target greenlet is present (its window
    // has streamed in): center that greenlet vertically, then clear the request.
    if (this.scrollToGid !== null) {
      const track = this.trackOf.get(this.scrollToGid);
      if (track !== undefined && this.rowOf[track] !== undefined) {
        const areaH = this.canvas.clientHeight - HEADER_H;
        this.scrollY =
          this.rowOf[track] * this.trackH - areaH / 2 + this.trackH / 2;
        this.scrollToGid = null;
      }
    }
    this.scrollY = Math.min(this.scrollY, this.maxScrollY());
    if (this.scrollY < 0) this.scrollY = 0;

    // Coalesced hover: run the (possibly long) pick at most once per frame, no
    // matter how many mousemove events fired since the last frame.
    if (this.pickPending) {
      this.setHover(this.pickAt(this.mouseX, this.mouseY));
      this.pickPending = false;
    }

    // Render-on-demand: skip the GL draw + 2D overlay unless something visible
    // changed. Always draw while following (the live edge moves), selecting (the
    // marquee tracks the cursor), the cursor just moved (crosshair/hover), or a
    // slow-log highlight is still flashing; also when the view scalars moved; and
    // ~2×/s regardless, so a missed wake can degrade only to mild staleness.
    const flashing = this.hl !== null && nowWall - this.hl.at < 900;
    const viewMoved =
      this.viewT0 !== this.lastDrawViewT0 ||
      this.pxPerMs !== this.lastDrawPx ||
      this.scrollY !== this.lastDrawScrollY;
    const shouldDraw =
      this.dirty ||
      this.follow ||
      this.selecting ||
      this.mouseMoved ||
      flashing ||
      viewMoved ||
      ++this.idleFrames >= 30;

    if (shouldDraw) {
      gl.viewport(0, 0, w, h);
      gl.clear(gl.COLOR_BUFFER_BIT);
      if (this.count > 0) {
        gl.useProgram(this.prog);
        gl.bindVertexArray(this.vao);
        gl.uniform2f(this.u.u_res!, w, h);
        // Rebase viewT0 into the loaded window's coordinate space (matches the
        // window-relative `a_start` we uploaded) — done in f64 here, so the f32 the
        // shader receives is a small number near the view, preserving precision.
        gl.uniform1f(this.u.u_viewT0!, this.viewT0 - this.glBaseMs);
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
      this.dirty = false;
      this.mouseMoved = false;
      this.idleFrames = 0;
      this.lastDrawViewT0 = this.viewT0;
      this.lastDrawPx = this.pxPerMs;
      this.lastDrawScrollY = this.scrollY;
    }
    this.rafId = requestAnimationFrame(this.frame);
  };

  // Full-width CPU-busy area graph, time-aligned with the executions below (same
  // viewT0/pxPerMs), read from the prebuilt bin histogram so cost is ~O(width).
  // The metric is the single gevent OS-thread's busy fraction (non-Hub run
  // time); 100% = that thread is CPU-bound. Not machine-wide / multi-core.
  private drawCpu(cw: number) {
    const ctx = this.ctx;
    const top = RULER_H;
    // The band is [top, top+CPU_H]. 0% sits exactly on the band's bottom divider
    // (top+CPU_H) so the baseline lines up with the background grid; the GAP_H
    // gap below it keeps the area clear of the first track. 100% is inset from
    // the top for the header row.
    const plotTop = top + 22;
    const plotBot = top + CPU_H;
    const plotH = plotBot - plotTop;
    const yOf = (f: number) => plotBot - Math.min(1, Math.max(0, f)) * plotH;

    ctx.fillStyle = this.tBg; // same as track background
    ctx.fillRect(0, top, cw, CPU_H);

    // Per-pixel busy fraction = busy run-time landing in that pixel's time span
    // ÷ the pixel's wall span (true utilization / duty cycle), computed exactly
    // by distributing each bin's busy-ms across the pixels it covers. Empty bins
    // are skipped, so an idle service stays cheap. When zoomed out (a pixel is a
    // slice of time) a moving average smooths the inherent per-pixel spikiness;
    // zoomed in, values are exact so a real burst still reads its true 100%.
    const col = (this.scrCpuCol = this.fitScr(this.scrCpuCol, cw));
    col.fill(0, 0, cw);
    const visMs = cw / this.pxPerMs;
    const firstBin = Math.max(0, Math.floor(this.viewT0 / BIN_MS));
    const lastBin = Math.min(
      this.nBins - 1,
      Math.floor((this.viewT0 + visMs) / BIN_MS),
    );
    if (lastBin >= firstBin) {
      const binPx = BIN_MS * this.pxPerMs;
      const busy = (this.scrCpuBusy = this.fitScr(this.scrCpuBusy, cw));
      busy.fill(0, 0, cw); // busy-ms accumulated per pixel column
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
        const pref = (this.scrCpuPref = this.fitScr(this.scrCpuPref, cw + 1));
        pref[0] = 0;
        for (let x = 0; x < cw; x++) pref[x + 1] = pref[x] + col[x];
        for (let x = 0; x < cw; x++) {
          const a = Math.max(0, x - w);
          const e = Math.min(cw, x + w + 1);
          col[x] = (pref[e] - pref[a]) / (e - a);
        }
      }
    }

    // Live CPU tail: in the pending area (right of the execution data edge) the
    // histogram has no data yet, so plot the live on-CPU samples (head-fed),
    // hold-interpolated — keeping the band moving at the live edge the way the lag
    // band does, instead of dropping to a flat baseline over the arrival-lag gap.
    const ncpu = this.cpuT.length;
    if (ncpu > 0 && !isNaN(this.t0ns)) {
      const edgeMs = this.fullSpanMs; // execution data edge in trace-ms
      let si = 0;
      let held = 0;
      for (let x = 0; x < cw; x++) {
        const t = this.viewT0 + x / this.pxPerMs;
        while (si < ncpu && this.cpuT[si] <= t) held = this.cpuV[si++];
        if (t > edgeMs) col[x] = held; // pending region → live sample
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
    ctx.fillStyle = this.rgba(this.tCpu, 0.18);
    ctx.fill();
    ctx.beginPath();
    for (let x = 0; x < cw; x++) {
      const y = yOf(col[x]);
      if (x === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.strokeStyle = this.tCpu;
    ctx.lineWidth = 1;
    ctx.stroke();

    // Header row: label on the left, current % on the right (no overlap with
    // the 100% guide, which is below this row).
    const hy = top + 9;
    ctx.font = "10px ui-monospace, Menlo, monospace";
    ctx.textBaseline = "middle";
    ctx.fillStyle = this.tMuted;
    const cpuLabel = "CPU · hub thread";
    ctx.fillText(cpuLabel, AXIS_X, hy);
    this.drawHelpBadge(
      AXIS_X + ctx.measureText(cpuLabel).width + 12,
      hy,
      CPU_HELP,
    );
    const pct = Math.round(this.cpuBusy(1000) * 100);
    ctx.font = "600 12px ui-monospace, Menlo, monospace";
    ctx.fillStyle = pct > 85 ? this.tBlock : this.tCpu;
    ctx.textAlign = "right";
    ctx.fillText(`${pct}%`, cw - 6, hy);
    ctx.textAlign = "left";

    // Hover readout: detailed % (and time) at the cursor while over the band.
    if (this.mouseX >= 0 && this.mouseY >= top && this.mouseY < top + CPU_H) {
      const x = Math.max(0, Math.min(cw - 1, Math.round(this.mouseX)));
      const v = col[x];
      const y = yOf(v);
      ctx.fillStyle = this.tWarn;
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
      ctx.fillStyle = this.tWarn;
      ctx.fillText(label, bx, by);
    }
  }

  /** Draw a small "?" badge centered at (x, cy) and register its hit rect for the
   *  hover tooltip (see [`drawHelpTooltip`]). Called from each band header. */
  private drawHelpBadge(x: number, cy: number, text: string) {
    const ctx = this.ctx;
    const r = 6;
    const over =
      this.mouseX >= 0 &&
      Math.hypot(this.mouseX - x, this.mouseY - cy) <= r + 2;
    ctx.beginPath();
    ctx.arc(x, cy, r, 0, Math.PI * 2);
    ctx.fillStyle = over ? "#4c566a" : "#262a33";
    ctx.fill();
    ctx.strokeStyle = "#3b4252";
    ctx.lineWidth = 1;
    ctx.stroke();
    ctx.fillStyle = this.tText2;
    ctx.font = "700 9px ui-monospace, Menlo, monospace";
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    // Centered both ways: textAlign center + textBaseline middle put the glyph in
    // the circle without per-font baseline fudging.
    ctx.fillText("?", x, cy);
    ctx.textAlign = "left";
    this.helpRects.push({ x, y: cy, r, text });
  }

  /** If the cursor is over a "?" badge, draw its explanation as a word-wrapped box.
   *  Drawn last so it sits above the bands. */
  private drawHelpTooltip(cw: number) {
    const hit = this.helpRects.find(
      (h) =>
        this.mouseX >= 0 &&
        Math.hypot(this.mouseX - h.x, this.mouseY - h.y) <= h.r + 2,
    );
    if (!hit) return;
    const ctx = this.ctx;
    ctx.font = AXIS_FONT;
    const maxW = 320;
    // Word-wrap to maxW.
    const words = hit.text.split(" ");
    const lines: string[] = [];
    let line = "";
    for (const w of words) {
      const t = line ? `${line} ${w}` : w;
      if (ctx.measureText(t).width > maxW && line) {
        lines.push(line);
        line = w;
      } else {
        line = t;
      }
    }
    if (line) lines.push(line);
    const lh = 16;
    const padX = 11;
    const padY = 9;
    const boxW =
      Math.min(maxW, Math.max(...lines.map((l) => ctx.measureText(l).width))) +
      padX * 2;
    const boxH = lines.length * lh + padY * 2;
    let bx = hit.x + 10;
    if (bx + boxW > cw) bx = cw - boxW - 4;
    if (bx < 4) bx = 4;
    const by = hit.y + 10;
    const radius = 6;
    ctx.beginPath();
    ctx.roundRect(bx, by, boxW, boxH, radius);
    ctx.fillStyle = "rgba(28,31,38,0.98)";
    ctx.fill();
    ctx.strokeStyle = "#3b4252";
    ctx.lineWidth = 1;
    ctx.stroke();
    ctx.fillStyle = this.tText2;
    ctx.textAlign = "left";
    ctx.textBaseline = "middle";
    for (let i = 0; i < lines.length; i++) {
      // Each line centered in its own `lh` slot, top slot offset by padY.
      ctx.fillText(lines[i], bx + padX, by + padY + i * lh + lh / 2);
    }
  }

  // Kernel scheduler-lag band (R13), directly under the CPU band and on the same
  // time axis. Plots the hub thread's run-queue-wait rate (ms of starvation per
  // wall-second) sampled from `/proc/.../schedstat`. A tall band = the OS was not
  // scheduling the hub (oversubscription / k8s CFS throttling) — so a long execution
  // here was off-CPU *waiting to run*, not your code being slow. Linux-only; the
  // band stays empty (just its label) where lag is unavailable.
  private drawLag(cw: number) {
    const ctx = this.ctx;
    const top = RULER_H + CPU_H;
    const plotTop = top + 16; // header row reserved at the band top
    const plotBot = top + LAG_H; // sits exactly on the band's bottom divider
    const plotH = plotBot - plotTop;
    const yOf = (f: number) => plotBot - Math.min(1, Math.max(0, f)) * plotH;

    ctx.fillStyle = this.tBg;
    ctx.fillRect(0, top, cw, LAG_H);

    // Per-pixel lag (ms/s), hold-interpolated from the sparse (~10/s) samples — so
    // the area, line, and hover readout all read the exact same column and stay
    // pixel-aligned with the executions below (same viewT0 / pxPerMs as everything else).
    const n = this.lagT.length;
    const col = (this.scrLagCol = this.fitScr(this.scrLagCol, cw));
    col.fill(0, 0, cw);
    if (n > 0 && !isNaN(this.t0ns)) {
      let si = 0;
      let held = 0; // value carried from the last sample at/left of this pixel
      for (let x = 0; x < cw; x++) {
        const t = this.viewT0 + x / this.pxPerMs;
        while (si < n && this.lagT[si] <= t) held = this.lagV[si++];
        col[x] = held;
      }
    }
    const cur = n > 0 ? col[cw - 1] : 0; // latest (right-edge) value for the header

    // Auto-scale the band to the tallest lag now in view so both quiet and
    // starved regimes stay legible; floored so near-zero jitter doesn't fill it.
    let peak = 0;
    for (let x = 0; x < cw; x++) if (col[x] > peak) peak = col[x];
    const fullScale = Math.max(peak, LAG_MIN_FULL_MS_S);

    // Baseline guide (0 lag) — on the band's bottom divider.
    ctx.strokeStyle = "rgba(120,130,150,0.12)";
    ctx.beginPath();
    ctx.moveTo(0, plotBot + 0.5);
    ctx.lineTo(cw, plotBot + 0.5);
    ctx.stroke();

    if (n > 0) {
      // Area.
      ctx.beginPath();
      ctx.moveTo(0, plotBot);
      for (let x = 0; x < cw; x++) ctx.lineTo(x, yOf(col[x] / fullScale));
      ctx.lineTo(cw, plotBot);
      ctx.closePath();
      ctx.fillStyle = this.rgba(this.tBlock, 0.18); // red-ish: starvation
      ctx.fill();
      // Line.
      ctx.beginPath();
      for (let x = 0; x < cw; x++) {
        const y = yOf(col[x] / fullScale);
        if (x === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
      ctx.strokeStyle = this.tBlock;
      ctx.lineWidth = 1;
      ctx.stroke();

      // Full-scale marker: the band auto-scales, so label what its top now means.
      ctx.font = "9px ui-monospace, Menlo, monospace";
      ctx.fillStyle = this.rgba(this.tMuted, 0.7);
      ctx.textAlign = "left";
      ctx.textBaseline = "top";
      ctx.fillText(`${Math.round(fullScale)} ms/s`, AXIS_X, plotTop + 1);
    }

    // Header row: label + latest rate.
    const hy = top + 8;
    ctx.font = "10px ui-monospace, Menlo, monospace";
    ctx.textBaseline = "middle";
    ctx.textAlign = "left";
    ctx.fillStyle = this.tMuted;
    const labelText = "Linux Scheduler Lag · hub thread";
    ctx.fillText(labelText, AXIS_X, hy);
    this.drawHelpBadge(
      AXIS_X + ctx.measureText(labelText).width + 12,
      hy,
      LAG_HELP,
    );
    ctx.font = "600 12px ui-monospace, Menlo, monospace";
    // Severity: <5 healthy (neutral), 5–50 contention (amber), ≥50 serious (red).
    ctx.fillStyle =
      cur >= 50 ? this.tBlock : cur >= 5 ? this.tWarn : this.tMuted;
    ctx.textAlign = "right";
    ctx.fillText(n > 0 ? `${cur.toFixed(1)} ms/s` : "n/a", cw - 6, hy);
    ctx.textAlign = "left";

    // Hover readout: lag value + time at the cursor while over the band.
    if (
      n > 0 &&
      this.mouseX >= 0 &&
      this.mouseY >= top &&
      this.mouseY < top + LAG_H
    ) {
      const x = Math.max(0, Math.min(cw - 1, Math.round(this.mouseX)));
      const v = col[x];
      const y = yOf(v / fullScale);
      ctx.fillStyle = this.tBlock;
      ctx.beginPath();
      ctx.arc(x, y, 3, 0, Math.PI * 2);
      ctx.fill();
      const t = this.viewT0 + this.mouseX / this.pxPerMs;
      const label = `${v.toFixed(1)} ms/s  ·  ${this.formatAxis(t)}`;
      ctx.font = AXIS_FONT;
      const tw = ctx.measureText(label).width;
      let bx = x + 10;
      if (bx + tw + 8 > cw) bx = x - tw - 14;
      const by = Math.max(top + 13, y - 16);
      ctx.fillStyle = "rgba(33,37,46,0.97)";
      ctx.fillRect(bx - 4, by - 9, tw + 8, 18);
      ctx.strokeStyle = "#3b4252";
      ctx.strokeRect(bx - 4, by - 9, tw + 8, 18);
      ctx.fillStyle = this.tBlock;
      ctx.fillText(label, bx, by);
    }
  }

  private drawOverlay(cw: number, ch: number, sx: number, sy: number) {
    const ctx = this.ctx;
    ctx.setTransform(sx, 0, 0, sy, 0, 0);
    ctx.clearRect(0, 0, cw, ch);

    ctx.fillStyle = this.tBg;
    ctx.fillRect(0, 0, cw, RULER_H);

    this.helpRects = []; // rebuilt by the band headers below
    this.drawCpu(cw);
    this.drawLag(cw);

    // Subtle dividers between ruler / CPU band / lag band / track area.
    ctx.strokeStyle = "rgba(255,255,255,0.06)";
    for (const y of [
      RULER_H,
      RULER_H + CPU_H,
      RULER_H + CPU_H + LAG_H,
      HEADER_H,
    ]) {
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

    // Anchor gridlines to round values of the DISPLAYED axis so a line falls on
    // the start of each second. In relative mode that's the trace origin (the
    // nice steps already divide/multiply 1000ms); in clock mode the origin's
    // wall-clock time isn't a round second, so shift by the epoch's sub-step
    // remainder to land lines on whole clock seconds.
    const clock = this.timeMode !== "relative" && Number.isFinite(this.epochMs);
    const phaseFor = (s: number) =>
      clock ? -(((this.epochMs % s) + s) % s) : 0;
    const gridStart = (s: number, phase: number) =>
      Math.ceil((this.viewT0 - phase) / s) * s + phase;
    const minorPhase = phaseFor(minor);
    const stepPhase = phaseFor(step);

    // minor lines (batched into one path)
    ctx.strokeStyle = "rgba(120,130,150,0.07)";
    ctx.beginPath();
    for (let t = gridStart(minor, minorPhase); ; t += minor) {
      const x = (t - this.viewT0) * this.pxPerMs;
      if (x > cw) break;
      if (x < 0) continue;
      ctx.moveTo(x + 0.5, RULER_H);
      ctx.lineTo(x + 0.5, ch);
    }
    ctx.stroke();

    // major lines + labels
    ctx.strokeStyle = "rgba(130,140,160,0.22)";
    for (let t = gridStart(step, stepPhase); ; t += step) {
      const x = (t - this.viewT0) * this.pxPerMs;
      if (x > cw) break;
      ctx.beginPath();
      ctx.moveTo(x + 0.5, RULER_H);
      ctx.lineTo(x + 0.5, ch);
      ctx.stroke();
      ctx.fillStyle = "#9aa3b2";
      ctx.fillText(this.formatAxis(t), x + 4, RULER_H / 2);
    }

    // Pre-start: the scroll buffer before the trace began (t < 0) has no data, so
    // tint it muted too — distinguishes "nothing here" from blank scrollable void.
    {
      const xOrigin = (0 - this.viewT0) * this.pxPerMs;
      if (xOrigin > 0) {
        ctx.fillStyle = "rgba(120,130,150,0.10)";
        ctx.fillRect(0, RULER_H, Math.min(xOrigin, cw), ch - RULER_H);
      }
    }

    // Loading region: visible time that DOES have data on the server but isn't in
    // the client's current window yet — you panned/zoomed to a section that was
    // dropped from memory, or its fetch is still in flight. Painted with the same
    // muted diagonal hatch as the pending live edge, so an unloaded section reads
    // as "loading…" rather than empty, and fills with real executions when the window
    // arrives. Restricted to [origin, newest] so it never overlaps the pre-start
    // or the live-edge pending region (each keeps its own treatment).
    {
      const visMs = cw / this.pxPerMs;
      const segL = Math.max(this.viewT0, 0);
      const segR = Math.min(this.viewT0 + visMs, this.fullSpanMs);
      const hatch = (aMs: number, bMs: number) => {
        const px0 = Math.max(0, (aMs - this.viewT0) * this.pxPerMs);
        const px1 = Math.min(cw, (bMs - this.viewT0) * this.pxPerMs);
        if (px1 <= px0) return;
        ctx.fillStyle = "rgba(120,130,150,0.10)";
        ctx.fillRect(px0, RULER_H, px1 - px0, ch - RULER_H);
        ctx.save();
        ctx.beginPath();
        ctx.rect(px0, RULER_H, px1 - px0, ch - RULER_H);
        ctx.clip();
        ctx.strokeStyle = "rgba(140,150,170,0.10)";
        for (let x = px0 - ch; x < px1; x += 9) {
          ctx.beginPath();
          ctx.moveTo(x, ch);
          ctx.lineTo(x + ch, RULER_H);
          ctx.stroke();
        }
        ctx.restore();
      };
      if (segR > segL) {
        if (this.loadedT1 < 0) {
          hatch(segL, segR); // nothing loaded yet (first window in flight)
        } else {
          const lL = (this.loadedT0 - this.originNs) / 1e6;
          const lR = (this.loadedT1 - this.originNs) / 1e6;
          if (lL > segL) hatch(segL, Math.min(lL, segR)); // unloaded on the left
          if (lR < segR) hatch(Math.max(lR, segL), segR); // unloaded on the right
        }
      }
    }

    // Pending region: from the newest captured span up to the live edge ("now").
    // This is the arrival lag — data that exists but hasn't streamed in yet — so
    // it's tinted muted (not blank) to read as "pending", with a "now" edge line.
    const liveEdge = this.liveEdgeMs();
    if (liveEdge > this.fullSpanMs) {
      const px0 = Math.max(0, (this.fullSpanMs - this.viewT0) * this.pxPerMs);
      const px1 = Math.min(cw, (liveEdge - this.viewT0) * this.pxPerMs);
      if (px1 > px0) {
        ctx.fillStyle = "rgba(120,130,150,0.10)";
        ctx.fillRect(px0, RULER_H, px1 - px0, ch - RULER_H);
        // diagonal hatch for a clear "no data here yet" texture
        ctx.save();
        ctx.beginPath();
        ctx.rect(px0, RULER_H, px1 - px0, ch - RULER_H);
        ctx.clip();
        ctx.strokeStyle = "rgba(140,150,170,0.10)";
        for (let x = px0 - ch; x < px1; x += 9) {
          ctx.beginPath();
          ctx.moveTo(x, ch);
          ctx.lineTo(x + ch, RULER_H);
          ctx.stroke();
        }
        ctx.restore();
        if (px1 <= cw) {
          ctx.strokeStyle = "rgba(160,170,190,0.55)"; // the "now" edge
          ctx.beginPath();
          ctx.moveTo(px1 + 0.5, RULER_H);
          ctx.lineTo(px1 + 0.5, ch);
          ctx.stroke();
        }
      }
    }

    // GC pauses: global vertical lines spanning the full height (a GC stalls the
    // whole gevent thread, so it blocks every greenlet at once). Hover shows
    // gen/duration/objects in the top readout. The layer is toggleable (the data
    // is still collected and counted; only its drawing is suppressed).
    if (this.showGc && !isNaN(this.t0ns)) {
      for (let i = 0; i < this.gcStart.length; i++) {
        const sMs = (this.gcStart[i] - this.t0ns) / 1e6;
        const x0 = (sMs - this.viewT0) * this.pxPerMs;
        // Min 2px so even sub-pixel pauses show as a consistent translucent band.
        const w = Math.max((this.gcDur[i] / 1e6) * this.pxPerMs, 2);
        if (x0 + w < 0 || x0 > cw) continue;
        ctx.fillStyle = this.rgba(this.tGc, 0.3);
        ctx.fillRect(x0, RULER_H, w, ch - RULER_H);
      }
    }

    // Highlighted execution (slow-log click): a bright full-height time band that
    // flashes briefly, then settles to a persistent outline + caret so the
    // clicked execution stays easy to spot after the window streams in around it.
    if (this.hl) {
      const x0 = (this.hl.t0Ms - this.viewT0) * this.pxPerMs;
      const bw = Math.max((this.hl.t1Ms - this.hl.t0Ms) * this.pxPerMs, 3);
      if (x0 + bw >= 0 && x0 <= cw) {
        const flash = Math.max(0, 1 - (performance.now() - this.hl.at) / 900);
        ctx.fillStyle = `rgba(136,192,208,${0.1 + 0.35 * flash})`;
        ctx.fillRect(x0, RULER_H, bw, ch - RULER_H);
        ctx.strokeStyle = "rgba(143,220,235,0.9)";
        ctx.lineWidth = 1.5;
        ctx.beginPath();
        ctx.moveTo(x0 + 0.5, RULER_H);
        ctx.lineTo(x0 + 0.5, ch);
        ctx.moveTo(x0 + bw - 0.5, RULER_H);
        ctx.lineTo(x0 + bw - 0.5, ch);
        ctx.stroke();
        ctx.lineWidth = 1;
        // Caret marker centered at the top of the band.
        const cx = x0 + bw / 2;
        ctx.fillStyle = "rgba(143,220,235,0.95)";
        ctx.beginPath();
        ctx.moveTo(cx - 5, RULER_H + 1);
        ctx.lineTo(cx + 5, RULER_H + 1);
        ctx.lineTo(cx, RULER_H + 8);
        ctx.closePath();
        ctx.fill();
      }
    }

    // Selected span stays outlined while its trace panel (the right menu) is open,
    // so the selection is "held" regardless of cursor position — cleared on close.
    // Hover gets a brighter transient outline on top (what the tooltip describes).
    if (this.selectedSpan)
      this.outlineSpan(this.selectedSpan, cw, ch, "rgba(136,192,208,0.95)", 2);
    if (this.hovered)
      this.outlineSpan(this.hovered, cw, ch, "rgba(255,255,255,0.92)", 1.5);

    if (this.mouseX >= 0 && this.mouseY >= RULER_H) {
      const t = this.viewT0 + this.mouseX / this.pxPerMs;
      ctx.strokeStyle = this.rgba(this.tWarn, 0.5);
      ctx.beginPath();
      ctx.moveTo(this.mouseX + 0.5, RULER_H);
      ctx.lineTo(this.mouseX + 0.5, ch);
      ctx.stroke();

      let label = this.formatAxis(t);
      const gi = this.gcAt(this.mouseX);
      if (gi >= 0) {
        label += `   ·   GC gen${this.gcGen[gi]} ${formatTimePrecise(this.gcDur[gi] / 1e6)} (${this.gcColl[gi]} freed)`;
      } else if (this.hovered) {
        label += `   ·   dur ${formatTimePrecise(this.hovered.durNs / 1e6)}`;
      }
      const tw = ctx.measureText(label).width;
      const bx = Math.min(this.mouseX + 8, cw - tw - 10);
      ctx.fillStyle = "rgba(33,37,46,0.95)";
      ctx.fillRect(bx - 4, 2, tw + 8, RULER_H - 4);
      ctx.fillStyle = this.tWarn;
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

    // Band-header "?" explanations, drawn last so they sit above everything.
    this.drawHelpTooltip(cw);
  }

  private installInput() {
    const cv = this.canvas;
    const signal = this.ac.signal; // remove every listener at once on dispose()
    cv.addEventListener(
      "wheel",
      (e) => {
        e.preventDefault();
        this.dirty = true;
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
      { passive: false, signal },
    );

    let dragging = false,
      lastX = 0,
      lastY = 0,
      downX = 0,
      downY = 0,
      moved = false;
    cv.addEventListener(
      "mousedown",
      (e) => {
        this.dirty = true;
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
      },
      { signal },
    );
    window.addEventListener(
      "mouseup",
      () => {
        this.dirty = true;
        if (this.selecting) {
          this.selecting = false;
          if (moved)
            this.zoomToRange(this.selStartX, this.mouseX); // drag = zoom
          else this.clickSelect(downX, downY); // click still selects execution
        } else if (dragging && !moved) {
          this.clickSelect(downX, downY); // a click selects the execution
        }
        dragging = false;
      },
      { signal },
    );
    cv.addEventListener(
      "mousemove",
      (e) => {
        this.mouseX = e.offsetX;
        this.mouseY = e.offsetY;
        this.mouseMoved = true; // wake the render loop (crosshair/overlays follow)
        if (Math.abs(e.offsetX - downX) > 3 || Math.abs(e.offsetY - downY) > 3)
          moved = true;
        if (this.selecting) {
          this.pickPending = false;
          return; // marquee is drawn from selStartX..mouseX in the frame loop
        }
        if (dragging) {
          this.viewT0 -= (e.offsetX - lastX) / this.pxPerMs;
          this.scrollY = Math.max(0, this.scrollY - (e.offsetY - lastY));
          lastX = e.offsetX;
          lastY = e.offsetY;
          // only a horizontal drag (time pan) cancels follow; vertical = list scroll
          if (Math.abs(e.offsetX - downX) > 3) this.follow = false;
          this.pickPending = false;
          this.setHover(null);
        } else {
          // Defer the (potentially long) pick to the frame loop — at most once per
          // frame, not once per mousemove event.
          this.pickPending = true;
        }
      },
      { signal },
    );
    cv.addEventListener(
      "mouseleave",
      () => {
        this.mouseX = this.mouseY = -1;
        this.pickPending = false;
        this.dirty = true; // one more frame to clear the crosshair/marquee
        this.setHover(null);
      },
      { signal },
    );
    cv.addEventListener("dblclick", () => this.fit(), { signal });
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

  /** Select the span at a screen point (opens the trace panel) and drop any
   *  lingering slow-log highlight band — clicking a span replaces that "locked"
   *  reveal so the old highlight doesn't stick around on a different span. The
   *  selected span stays outlined until the panel closes (see clearSelection). */
  private clickSelect(px: number, py: number) {
    this.hl = null;
    this.dirty = true;
    const h = this.pickAt(px, py);
    this.selectedSpan = h;
    this.onSelect(h);
  }

  /** Drop the held selection outline — called when the trace panel closes. */
  clearSelection() {
    this.selectedSpan = null;
    this.dirty = true;
  }

  /** Outline a span (clipped to the track area) — recomputed from its absolute
   *  time + row so it tracks pan/zoom/scroll. Shared by hover + held selection. */
  private outlineSpan(
    h: Hover,
    cw: number,
    ch: number,
    stroke: string,
    lw: number,
  ) {
    if (isNaN(this.t0ns)) return;
    const track = this.trackOf.get(h.gid);
    const row = track !== undefined ? this.rowOf[track] : undefined;
    if (row === undefined) return;
    const sMs = (h.startNs - this.t0ns) / 1e6;
    const x = (sMs - this.viewT0) * this.pxPerMs;
    const w = Math.max((h.durNs / 1e6) * this.pxPerMs, MIN_PX);
    const y = HEADER_H + row * this.trackH - this.scrollY;
    const hgt = this.trackH - 1;
    if (x + w < 0 || x > cw || y + hgt < HEADER_H || y > ch) return;
    const ctx = this.ctx;
    ctx.save();
    ctx.beginPath();
    ctx.rect(0, HEADER_H, cw, ch - HEADER_H);
    ctx.clip();
    ctx.strokeStyle = stroke;
    ctx.lineWidth = lw;
    ctx.strokeRect(x - 0.5, y - 0.5, w + 1, hgt + 1);
    ctx.restore();
    ctx.lineWidth = 1;
  }

  /** The execution (with detail) at a screen position, or null. */
  private hoverFromIndex(i: number, px: number, py: number): Hover {
    const track = this.cTrack[i];
    return {
      name: this.trackName[track],
      gid: this.cGid[i],
      // Integer ns: the detail request/reply key off these, and the server parses
      // them as u64 (a fractional value would be rejected → no detail).
      startNs: Math.round(this.cStart[i] * 1e6 + this.t0ns),
      durNs: Math.round(this.cDur[i] * 1e6),
      // Render-only frame carries no func/task/stack — the consumer fetches them
      // lazily by (gid, startNs) via the `detail` request and fills them in.
      func: "",
      task: "",
      stack: "",
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
    // The buffer is time-ordered by start; scanning newest→oldest, once a span
    // starts more than the longest buffered duration (+ pick slop) before the
    // cursor, no earlier span can still cover it — stop. Bounds the scan to the
    // cursor's time neighborhood instead of the whole (up to 200k-row) window.
    const floor = t - Math.max(this.maxDurMs, slopMs);
    for (let i = this.count - 1; i >= 0; i--) {
      const s = this.cStart[i];
      if (s < floor) break;
      if (this.cTrack[i] !== track) continue;
      if (t >= s && t <= s + Math.max(this.cDur[i], slopMs)) {
        return this.hoverFromIndex(i, px, py);
      }
    }
    return null;
  }

  /** Jump the view to a execution (center it, zoom to frame it, select it). */
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
   *  matter how many greenlets exist. Each carries its greenlet color and total
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
      // Swatch follows the active color mode: identity hue in "ident"; the
      // greenlet's worst span's tier/heat color in "duration"/"heatmap" (Hub green).
      const [r, g, b] = this.fillRgb(track, this.trackMaxDur[track] || 0);
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
