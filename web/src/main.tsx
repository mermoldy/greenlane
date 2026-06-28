import { StrictMode, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Timeline,
  formatTime,
  type Hover,
  type Slice,
  type SortMode,
} from "./timeline.ts";
import "./styles.css";

type Source = { file: string; bytes: number };
type GcEvent = { start: number; dur: number; gen: number; collected: number };
type SlowRow = {
  start: number;
  dur: number;
  gid: number;
  name: string;
  func: string;
  level: number;
};

type WsMsg =
  // Session metadata (once on connect): identity, the fixed time origin, and the
  // whole-capture span/counts so the viewer can fit/follow without holding data.
  | {
      type: "meta";
      pid: number;
      epochMs: number | null;
      live: boolean;
      source: Source | null;
      originNs: number;
      spanNs: number;
      totalSlices: number;
      bytes: number;
    }
  // Reply to a viewport request: only the slices/GC overlapping the visible range.
  | {
      type: "window";
      t0: number;
      t1: number;
      slices: Slice[];
      gc: GcEvent[];
      counts: { visible: number; total: number };
      capped: boolean;
      spanNs: number;
      bytes: number;
    }
  // Live edge advance (so follow keeps moving and the header stays current).
  | { type: "head"; spanNs: number; totalSlices: number; bytes: number }
  | { type: "slowlog"; rows: SlowRow[] }
  | { type: "stats"; p50: number; p95: number; p99: number }
  | { type: "status"; live: boolean };

// Bytes → a compact human size, e.g. 1.4 MB.
function formatBytes(n: number): string {
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
function formatRate(r: number): string {
  if (r >= 1000) return `${(r / 1000).toFixed(1)}k/s`;
  return `${r.toFixed(r < 10 ? 1 : 0)}/s`;
}

function sortTitle(sort: SortMode): string {
  switch (sort) {
    case "recent1":
      return "Order lanes by scheduler activity in the most recent 1 second.";
    case "recent10":
      return "Order lanes by scheduler activity in the most recent 10 seconds.";
    case "recent60":
      return "Order lanes by scheduler activity in the most recent 60 seconds.";
    case "activity":
      return "Order lanes by total run time across the whole capture.";
    case "ident":
      return "Order lanes by their stable runtime identity.";
  }
}

function timeModeTitle(mode: "relative" | "current" | "utc"): string {
  switch (mode) {
    case "relative":
      return "Show elapsed time from the beginning of the trace.";
    case "current":
      return "Show local wall-clock time using the trace start timestamp.";
    case "utc":
      return "Show UTC wall-clock time using the trace start timestamp.";
  }
}

function App() {
  const glRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLCanvasElement>(null);
  const tlRef = useRef<Timeline | null>(null);
  const [connected, setConnected] = useState(false);
  const [live, setLive] = useState(true); // session live (vs detached)
  const [pid, setPid] = useState<number | null>(null);
  const [tmode, setTmode] = useState<"relative" | "current" | "utc">(
    "relative",
  );
  const [total, setTotal] = useState(0); // whole-capture slice count (server)
  const [tracks, setTracks] = useState(0);
  const [gc, setGc] = useState(0);
  const [rate, setRate] = useState(0);
  const [p95, setP95] = useState(0); // non-Hub duration p95 (ns), from the DB
  const [capped, setCapped] = useState(false); // window hit the slice cap
  const [source, setSource] = useState<Source | null>(null);
  // Server-authoritative running totals, held in refs and surfaced via the poll
  // so they don't re-render per message.
  const [dataBytes, setDataBytes] = useState(0);
  const dataBytesRef = useRef(0);
  const totalRef = useRef(0);
  const wsRef = useRef<WebSocket | null>(null);
  const [drag, setDrag] = useState<"pan" | "zoom">("zoom");
  const [helpOpen, setHelpOpen] = useState(false);
  const [slowOpen, setSlowOpen] = useState(false);
  const [slowLevel, setSlowLevel] = useState<"all" | "warn" | "red">("all");
  const [slowSort, setSlowSort] = useState<"time" | "dur">("time");
  const [slowRows, setSlowRows] = useState<SlowRow[]>([]);
  const [follow, setFollow] = useState(true);
  const [zoom, setZoom] = useState(1); // pxPerMs
  const [sort, setSort] = useState<SortMode>("recent1");
  const [headerH, setHeaderH] = useState(0);
  const [rowH, setRowH] = useState(18);
  const [hover, setHover] = useState<Hover | null>(null);
  const [selected, setSelected] = useState<Hover | null>(null);
  const [editor, setEditorState] = useState<string>(
    () => localStorage.getItem("gl.editor") || "vscode",
  );
  const setEditor = (e: string) => {
    setEditorState(e);
    localStorage.setItem("gl.editor", e);
  };
  const [labels, setLabels] = useState<
    { name: string; y: number; color: string; runMs: number }[]
  >([]);

  useEffect(() => {
    const tl = new Timeline(glRef.current!, overlayRef.current!);
    tlRef.current = tl;
    tl.onHover = setHover;
    tl.onSelect = setSelected;
    // The visible range changed → ask the server for exactly that window.
    tl.onViewport = (t0, t1, px) => {
      const ws = wsRef.current;
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "viewport", t0, t1, px }));
      }
    };
    setHeaderH(tl.headerHeight());
    setRowH(tl.rowHeight());
    const poll = setInterval(() => {
      setTotal(totalRef.current);
      setTracks(tl.nTracks);
      setFollow(tl.follow);
      setZoom(tl.pxPerMs);
      setSort(tl.sortMode);
      setGc(tl.gcCount());
      // Rate over the WHOLE capture (window count would understate it).
      setRate(tl.fullSpanMs > 0 ? totalRef.current / (tl.fullSpanMs / 1000) : 0);
      setDataBytes(dataBytesRef.current);
      setLabels(tl.trackLabels());
    }, 150);
    return () => clearInterval(poll);
  }, []);

  // Slow log + p95 are server-side DB queries. While the panel is open, (re)issue
  // the slow-log query on level/sort change and on a timer; p95 refreshes always.
  useEffect(() => {
    const send = (m: object) => {
      const ws = wsRef.current;
      if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(m));
    };
    const stats = () => send({ type: "stats" });
    const slow = () =>
      send({ type: "slowlog", level: slowLevel, sort: slowSort, limit: 500 });
    stats();
    if (slowOpen) slow();
    const id = setInterval(() => {
      stats();
      if (slowOpen) slow();
    }, 1000);
    return () => clearInterval(id);
  }, [slowOpen, slowLevel, slowSort]);

  useEffect(() => {
    let ws: WebSocket;
    let stop = false;
    const connect = () => {
      const proto = location.protocol === "https:" ? "wss" : "ws";
      ws = new WebSocket(`${proto}://${location.host}/ws`);
      wsRef.current = ws;
      ws.onopen = () => setConnected(true);
      ws.onclose = () => {
        setConnected(false);
        if (!stop) setTimeout(connect, 1000);
      };
      ws.onmessage = (e) => {
        const msg: WsMsg = JSON.parse(e.data);
        const tl = tlRef.current;
        switch (msg.type) {
          case "meta":
            setPid(msg.pid);
            setLive(msg.live);
            setSource(msg.source);
            dataBytesRef.current = msg.bytes;
            totalRef.current = msg.totalSlices;
            if (tl) {
              tl.epochMs = msg.epochMs ?? NaN;
              tl.setOrigin(msg.originNs);
              tl.setSpan(msg.spanNs);
              // A recording is static: stop following and fit the whole span so
              // the first window covers it. Live keeps following the edge.
              if (!msg.live) {
                tl.follow = false;
                setFollow(false);
                tl.fit();
              }
            }
            break;
          case "window":
            // Drop the previous window and render only this one (bounded memory).
            dataBytesRef.current = msg.bytes;
            totalRef.current = msg.counts.total;
            setCapped(msg.capped);
            tl?.setSpan(msg.spanNs);
            tl?.replaceWindow(msg.slices, msg.gc);
            break;
          case "head":
            dataBytesRef.current = msg.bytes;
            totalRef.current = msg.totalSlices;
            tl?.setSpan(msg.spanNs);
            break;
          case "slowlog":
            setSlowRows(msg.rows);
            break;
          case "stats":
            setP95(msg.p95);
            break;
          case "status":
            setLive(msg.live);
            if (!msg.live) freeze();
            break;
        }
      };
    };
    connect();
    return () => {
      stop = true;
      ws?.close();
    };
  }, []);

  const toggleFollow = () => {
    const tl = tlRef.current;
    if (!tl) return;
    tl.follow = !tl.follow;
    setFollow(tl.follow);
  };

  // On detach, stop follow so the timeline freezes (no further animation).
  const freeze = () => {
    if (tlRef.current) tlRef.current.follow = false;
    setFollow(false);
  };

  return (
    <div className="app">
      <div className="topbar">
        <span
          className="title"
          title="greenlane: live timeline profiler for gevent and asyncio scheduler activity"
        >
          <IconLane /> greenlane
        </span>
        <span
          className="stat"
          title={
            !connected
              ? "The viewer is not connected to the greenlane WebSocket."
              : source
                ? "Viewing a saved .glr recording. The timeline is static."
                : live
                  ? "Connected to a live target process and still receiving events."
                  : "Detached from the target. The trace is no longer collecting new events."
          }
        >
          <span
            className={`dot ${connected ? (source ? "file" : live ? "live" : "dead") : "dead"}`}
          />
          {!connected
            ? "disconnected"
            : source
              ? "recording"
              : live
                ? "live"
                : "detached"}
        </span>
        {source && (
          <span
            className="stat file"
            title={`Recording file: ${source.file}. Size: ${formatBytes(source.bytes)}.`}
          >
            <IconOpen />
            <span className="nm">{source.file.split("/").pop()}</span>
            <span>· {formatBytes(source.bytes)}</span>
          </span>
        )}
        {pid != null && (
          <span
            className="stat"
            title="Process ID of the target Python process this session attached to."
          >
            pid {pid}
          </span>
        )}
        <span
          className="stat"
          title="Closed run intervals collected so far (whole capture). Each slice is one continuous task or greenlet run."
        >
          {total.toLocaleString()} slices
        </span>
        {!source && (
          <span
            className="stat"
            title="Raw event-stream bytes received and processed by the server so far."
          >
            {formatBytes(dataBytes)}
          </span>
        )}
        <span
          className="stat"
          title="Mean scheduler events per second over the captured time span."
        >
          {formatRate(rate)}
        </span>
        <span
          className="stat"
          title="Number of lanes discovered in the trace. A lane is a gevent greenlet or asyncio task."
        >
          {tracks} lanes
        </span>
        <span
          className="stat gc"
          title="Garbage-collection pauses captured as global timeline markers."
        >
          {gc.toLocaleString()} GC
        </span>
        {p95 > 0 && (
          <span
            className="stat"
            title="95th-percentile run-interval duration (non-Hub), computed in the database over the whole capture."
          >
            p95 {formatTime(p95 / 1e6)}
          </span>
        )}
        {capped && (
          <span
            className="stat"
            title="The visible range has more slices than the render cap; zoom in to see them all."
            style={{ color: "#ebcb8b" }}
          >
            ⚠ capped
          </span>
        )}
        <label className="ctl" title={sortTitle(sort)}>
          sort
          <select
            value={sort}
            title={sortTitle(sort)}
            onChange={(e) => {
              const m = e.target.value as SortMode;
              tlRef.current?.setSortMode(m);
              setSort(m);
            }}
          >
            <option
              value="recent1"
              title="Put lanes with the most run time in the latest 1 second first."
            >
              activity (1s)
            </option>
            <option
              value="recent10"
              title="Put lanes with the most run time in the latest 10 seconds first."
            >
              activity (10s)
            </option>
            <option
              value="recent60"
              title="Put lanes with the most run time in the latest 60 seconds first."
            >
              activity (60s)
            </option>
            <option
              value="activity"
              title="Put lanes with the highest total run time first."
            >
              activity (total)
            </option>
            <option value="ident" title="Use stable runtime identity order.">
              ident
            </option>
          </select>
        </label>
        <label className="ctl" title={timeModeTitle(tmode)}>
          time
          <select
            value={tmode}
            title={timeModeTitle(tmode)}
            onChange={(e) => {
              const m = e.target.value as "relative" | "current" | "utc";
              tlRef.current?.setTimeMode(m);
              setTmode(m);
            }}
          >
            <option
              value="relative"
              title="Show time as elapsed duration since trace start."
            >
              relative
            </option>
            <option
              value="current"
              title="Show local clock time for each point on the trace."
            >
              current
            </option>
            <option value="utc" title="Show UTC clock time for the trace.">
              utc
            </option>
          </select>
        </label>
        <div className="right">
          <button
            className="danger"
            onClick={() => fetch("/detach", { method: "POST" }).catch(() => {})}
            disabled={!connected || !live}
            title="Stop instrumenting the target process and leave the current timeline frozen."
          >
            <IconDetach /> detach
          </button>
          <button
            className={follow ? "followon" : "followoff"}
            onClick={toggleFollow}
            title={
              follow
                ? "Following the live edge. Click to keep the current viewport in place."
                : "Follow is paused. Click to jump back to the live edge as new data arrives."
            }
          >
            <IconFollow /> {follow ? "following" : "follow off"}
          </button>
        </div>
      </div>
      <div className="stage">
        <canvas ref={glRef} />
        <canvas ref={overlayRef} className="overlay" />
        <div className="tracklabels" style={{ top: headerH }}>
          {labels.map((l, i) => (
            <div
              key={i}
              className="tracklabel"
              style={{
                top: l.y - headerH,
                height: rowH,
                lineHeight: `${rowH}px`,
              }}
            >
              <span className="dot2" style={{ background: l.color }} />
              <span className="nm">{l.name}</span>
              <span className="rt">{formatTime(l.runMs)}</span>
            </div>
          ))}
        </div>
        {hover && <Tooltip h={hover} />}
        {selected && (
          <TracePanel
            h={selected}
            onClose={() => setSelected(null)}
            editor={editor}
            onEditor={setEditor}
          />
        )}
      </div>
      {slowOpen && (
        <SlowLog
          rows={slowRows}
          level={slowLevel}
          sort={slowSort}
          onLevel={setSlowLevel}
          onSort={setSlowSort}
          onPick={(startNs, durNs) => tlRef.current?.revealSpanAt(startNs, durNs)}
          onClose={() => setSlowOpen(false)}
        />
      )}
      <div className="bottombar">
        <button
          className={`slowtoggle${slowOpen ? " on" : ""}`}
          onClick={() => setSlowOpen((v) => !v)}
          title="slow spans (>20ms), queried from the database"
        >
          slow log ({slowRows.length.toLocaleString()}) {slowOpen ? "▾" : "▸"}
        </button>
        <div className="bbright">
          <span className="seg" title="what dragging the timeline does">
            <button
              className={drag === "pan" ? "sel" : ""}
              onClick={() => {
                tlRef.current?.setDragMode("pan");
                setDrag("pan");
              }}
            >
              <IconHand /> pan
            </button>
            <button
              className={drag === "zoom" ? "sel" : ""}
              onClick={() => {
                tlRef.current?.setDragMode("zoom");
                setDrag("zoom");
              }}
            >
              <IconZoom /> zoom
            </button>
          </span>
          <div className="zoom">
            <input
              type="range"
              min={0}
              max={1}
              step={0.001}
              value={zoomToSlider(zoom)}
              onChange={(e) => {
                const px = sliderToZoom(Number(e.target.value));
                tlRef.current?.zoomTo(px);
                setZoom(px);
              }}
            />
            <span className="zval" title="time per pixel">
              {formatTime(1 / zoom)}/px
            </span>
            <button
              onClick={() => tlRef.current?.fit()}
              title="fit all to width"
            >
              <IconFit /> fit
            </button>
          </div>
          <button
            className={`help${helpOpen ? " on" : ""}`}
            onClick={() => setHelpOpen((v) => !v)}
            title="controls"
          >
            <IconHelp />
          </button>
        </div>
        {helpOpen && (
          <div className="helppop">
            <div className="hrow">
              <b>scroll</b> scroll greenlet list
            </div>
            <div className="hrow">
              <b>⌘/ctrl + scroll</b> zoom in/out
            </div>
            <div className="hrow">
              <b>shift + scroll</b> pan time
            </div>
            <div className="hrow">
              <b>drag</b> {drag === "zoom" ? "zoom to selection" : "pan time"}
            </div>
            <div className="hrow">
              <b>ruler-drag / shift+drag</b> zoom to selection
            </div>
            <div className="hrow">
              <b>click span</b> open trace
            </div>
            <div className="hrow">
              <b>double-click</b> fit all
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

// Bottom slow-log panel. Rows are queried from the database (the level/sort
// controls re-issue the query upstream); click a row to seek the timeline to it.
function SlowLog({
  rows,
  level,
  sort,
  onLevel,
  onSort,
  onPick,
  onClose,
}: {
  rows: SlowRow[];
  level: "all" | "warn" | "red";
  sort: "time" | "dur";
  onLevel: (l: "all" | "warn" | "red") => void;
  onSort: (s: "time" | "dur") => void;
  onPick: (startNs: number, durNs: number) => void;
  onClose: () => void;
}) {
  // "warn" = warn-level only (the DB returns warn+red for "all"/"warn").
  const shown = level === "warn" ? rows.filter((r) => r.level === 1) : rows;
  return (
    <div className="slowlog">
      <div className="slowlog-head">
        <span>slow log · {shown.length} shown</span>
        <span className="segwrap">
          show
          <span className="seg">
            <button className={level === "all" ? "sel" : ""} onClick={() => onLevel("all")}>
              all
            </button>
            <button className={level === "warn" ? "sel" : ""} onClick={() => onLevel("warn")}>
              warn
            </button>
            <button className={level === "red" ? "sel" : ""} onClick={() => onLevel("red")}>
              red
            </button>
          </span>
        </span>
        <span className="segwrap">
          sort
          <span className="seg">
            <button className={sort === "time" ? "sel" : ""} onClick={() => onSort("time")}>
              time
            </button>
            <button className={sort === "dur" ? "sel" : ""} onClick={() => onSort("dur")}>
              duration
            </button>
          </span>
        </span>
        <span className="muted">click a row to jump</span>
        <button onClick={onClose} title="close">
          <IconClose />
        </button>
      </div>
      <div className="slowlog-body">
        {shown.length === 0 && (
          <div className="slowrow muted">no spans over 20 ms yet</div>
        )}
        {shown.map((s, i) => (
          <div key={i} className="slowrow" onClick={() => onPick(s.start, s.dur)}>
            <span
              className="lvl"
              style={{ background: s.level >= 2 ? "#e8606b" : "#ebcb8b" }}
            />
            <span className="sdur" style={{ color: durColor(s.dur / 1e6) }}>
              {formatTime(s.dur / 1e6)}
            </span>
            <span className="snm">{s.name}</span>
            <span className="sfn">{s.func || "—"}</span>
            <span className="sat">+{formatTime(s.start / 1e6)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

// Duration → highlight color: yellow > 20ms, red > 50ms.
function durColor(ms: number): string | undefined {
  if (ms > 50) return "#e8606b";
  if (ms > 20) return "#ebcb8b";
  return undefined;
}

// A stack frame "fullpath:qualname:lineno" → its parts.
interface Frame {
  path: string;
  file: string;
  qual: string;
  line: string;
  lib: boolean;
}
function parseFrame(f: string): Frame {
  const parts = f.split(":");
  const line = parts.length >= 3 ? parts[parts.length - 1] : "";
  const qual =
    parts.length >= 3 ? parts.slice(1, -1).join(":") : (parts[1] ?? "");
  const path = parts[0] ?? f;
  const file = path.split("/").pop() || path;
  const lib = path.includes("/gevent/") || path.includes("/greenlet");
  return { path, file, qual, line, lib };
}

// Inline SVG icons (no external assets — CSP-safe).
const svg = (children: any) => (
  <svg
    className="ico"
    width="13"
    height="13"
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.6"
    strokeLinecap="round"
    strokeLinejoin="round"
  >
    {children}
  </svg>
);
// "Skip/stick to the latest" — a play triangle against an end bar.
const IconFollow = () => (
  <svg
    className="ico"
    width="13"
    height="13"
    viewBox="0 0 16 16"
    aria-hidden="true"
  >
    <path d="M3 3.5L9.5 8 3 12.5z" fill="currentColor" />
    <rect x="11" y="3.5" width="2" height="9" rx="0.6" fill="currentColor" />
  </svg>
);
const IconFit = () =>
  svg(
    <>
      <path d="M2 4v8" />
      <path d="M14 4v8" />
      <path d="M4 8h8" />
      <path d="M6 6L4 8l2 2" />
      <path d="M10 6l2 2-2 2" />
    </>,
  );
const IconZoom = () =>
  svg(
    <>
      <circle cx="7" cy="7" r="4.2" />
      <path d="M13.5 13.5l-3-3" />
    </>,
  );
const IconClose = () =>
  svg(
    <>
      <path d="M4 4l8 8" />
      <path d="M12 4l-8 8" />
    </>,
  );
const IconOpen = () =>
  svg(
    <>
      <path d="M6 3h7v7" />
      <path d="M13 3l-7 7" />
      <path d="M11 9v4H3V5h4" />
    </>,
  );
const IconHelp = () =>
  svg(
    <>
      <circle cx="8" cy="8" r="6.5" />
      <path d="M6.2 6.2a1.8 1.8 0 1 1 2.4 1.7c-.6.3-.9.6-.9 1.3" />
      <circle cx="8" cy="11.6" r="0.1" />
    </>,
  );
const IconHand = () =>
  svg(
    <>
      <path d="M6 7V4a1 1 0 0 1 2 0v3M8 7V3.4a1 1 0 0 1 2 0V7M10 7V4.4a1 1 0 0 1 2 0V10c0 2-1.5 3.5-3.7 3.5-1.4 0-2.5-.6-3.3-1.8L3.8 9a1 1 0 0 1 1.7-1l1 1.2" />
    </>,
  );
const IconDetach = () => (
  <svg
    className="ico"
    width="12"
    height="12"
    viewBox="0 0 16 16"
    aria-hidden="true"
  >
    <rect x="3.5" y="3.5" width="9" height="9" rx="1.6" fill="currentColor" />
  </svg>
);

// file:line → editor deep link.
function editorUrl(ed: string, path: string, line: string): string {
  const l = line || "1";
  if (ed === "pycharm")
    return `pycharm://open?file=${encodeURIComponent(path)}&line=${l}`;
  return `${ed}://file${path}:${l}`; // vscode, cursor, zed
}
const IconLane = () => (
  <svg
    className="ico"
    width="14"
    height="14"
    viewBox="0 0 16 16"
    aria-hidden="true"
  >
    <rect x="1" y="2.5" width="8" height="2.6" rx="1.3" fill="#a3be8c" />
    <rect x="5" y="6.7" width="10" height="2.6" rx="1.3" fill="#88c0d0" />
    <rect x="2" y="10.9" width="6" height="2.6" rx="1.3" fill="#a3be8c" />
  </svg>
);

// Persistent right-side panel with the full, detailed call trace for a span:
// every captured frame (incl. library), full file paths, function + line.
function TracePanel({
  h,
  onClose,
  editor,
  onEditor,
}: {
  h: Hover;
  onClose: () => void;
  editor: string;
  onEditor: (e: string) => void;
}) {
  const frames = (h.stack ? h.stack.split(" <- ") : h.func ? [h.func] : []).map(
    parseFrame,
  );
  const endNs = h.startNs + h.durNs;
  return (
    <div className="panel">
      <div className="panel-head">
        <span className="dot2" style={{ background: hueDot(h.name) }} />
        <span className="name">{h.name}</span>
        <button onClick={onClose} title="close">
          <IconClose />
        </button>
      </div>
      <div className="panel-body">
        <div className="row">
          <span className="k">duration</span>{" "}
          <span style={{ color: durColor(h.durNs / 1e6) }}>
            {formatTime(h.durNs / 1e6)}
          </span>
        </div>
        <div className="row">
          <span className="k">start</span> +{formatTime(h.startNs / 1e6)}
        </div>
        <div className="row">
          <span className="k">end</span> +{formatTime(endNs / 1e6)}
        </div>
        {h.task && (
          <div className="row">
            <span className="k">task</span> {h.task}
          </div>
        )}
        <div className="row">
          <span className="k">gid</span> 0x{h.gid.toString(16)}
        </div>
        <div className="trace-title">
          <span>call trace ({frames.length} frames · leaf → root)</span>
          <label className="ctl open-in">
            open in
            <select value={editor} onChange={(e) => onEditor(e.target.value)}>
              <option value="vscode">VS Code</option>
              <option value="cursor">Cursor</option>
              <option value="zed">Zed</option>
              <option value="pycharm">PyCharm</option>
            </select>
          </label>
        </div>
        <div className="trace">
          {frames.length === 0 && (
            <div className="frame">(no frames captured)</div>
          )}
          {frames.map((f, i) => (
            <a
              key={i}
              className={`tframe${i === 0 ? " leaf" : ""}${f.lib ? " lib" : ""}`}
              href={editorUrl(editor, f.path, f.line)}
              title={`open ${f.path}:${f.line || ""} in editor`}
            >
              <div className="tline">
                <span className="marker">{i === 0 ? "▸" : "↑"}</span>
                <span className="fn">{f.qual || f.file}</span>
                {f.line && <span className="ln">:{f.line}</span>}
                <span className="openico">
                  <IconOpen />
                </span>
              </div>
              <div className="tpath">{f.path}</div>
            </a>
          ))}
        </div>
      </div>
    </div>
  );
}

// A stable color dot for the panel header, by greenlet name.
function hueDot(name: string): string {
  if (/^hub/i.test(name)) return "#a3be8c";
  let h = 0;
  for (let i = 0; i < name.length; i++)
    h = (h * 31 + name.charCodeAt(i)) & 1023;
  const hue = h % 2 < 1 ? 25 + (h % 55) : 165 + (h % 165);
  return `hsl(${hue},55%,60%)`;
}

// Log-scale mapping between the zoom slider (0..1) and pxPerMs.
const LN_MIN = Math.log(Timeline.MIN_ZOOM);
const LN_MAX = Math.log(Timeline.MAX_ZOOM);
const zoomToSlider = (px: number) =>
  Math.min(1, Math.max(0, (Math.log(px) - LN_MIN) / (LN_MAX - LN_MIN)));
const sliderToZoom = (v: number) => Math.exp(LN_MIN + v * (LN_MAX - LN_MIN));

// Context-specific detail panel — the hook to enrich with stack samples,
// syscall annotations, links into your app's domain, etc.
function Tooltip({ h }: { h: Hover }) {
  // Flip to the left of the cursor when near the right edge so it never clips.
  const flip = h.x > window.innerWidth * 0.55;
  const style = flip
    ? { right: Math.max(8, window.innerWidth - h.x + 14), top: h.y + 14 }
    : { left: h.x + 14, top: h.y + 14 };
  return (
    <div className="tooltip" style={style}>
      <div className="name">{h.name}</div>
      <div>
        <span className="k">dur</span>{" "}
        <span style={{ color: durColor(h.durNs / 1e6) }}>
          {formatTime(h.durNs / 1e6)}
        </span>
      </div>
      <div>
        <span className="k">start</span> +{formatTime(h.startNs / 1e6)}
      </div>
      {h.task && (
        <div>
          <span className="k">task</span> {h.task}
        </div>
      )}
      <div>
        <span className="k">gid</span> 0x{h.gid.toString(16)}
      </div>
      <ShortStack h={h} />
    </div>
  );
}

// Short hover trace: app frames only (library hidden), basename, capped — the
// full detail (all frames, paths) lives in the click panel.
function ShortStack({ h }: { h: Hover }) {
  const all = (h.stack ? h.stack.split(" <- ") : h.func ? [h.func] : []).map(
    parseFrame,
  );
  const app = all.filter((f) => !f.lib);
  const frames = (app.length ? app : all).slice(0, 6);
  const hidden = (app.length ? app.length : all.length) - frames.length;
  if (frames.length === 0) return null;
  return (
    <div className="stack">
      {frames.map((f, i) => (
        <div key={i} className={i === 0 ? "frame leaf" : "frame"}>
          {i === 0 ? "▸ " : "↑ "}
          {f.qual || f.file}
          <span className="sloc">
            {" "}
            {f.file}
            {f.line && `:${f.line}`}
          </span>
        </div>
      ))}
      {hidden > 0 && (
        <div className="frame more">+{hidden} more · click for full trace</div>
      )}
    </div>
  );
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
