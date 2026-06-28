import { StrictMode, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { Timeline, formatTime, type Hover, type Slice, type SortMode } from "./timeline.ts";
import "./styles.css";

type WsMsg =
  | { type: "snapshot"; slices: Slice[] }
  | { type: "slices"; slices: Slice[] }
  | { type: "meta"; pid: number; epochMs: number | null; live: boolean }
  | { type: "status"; live: boolean }
  | { type: "gc"; events: { start: number; dur: number; gen: number; collected: number }[] };

function App() {
  const glRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLCanvasElement>(null);
  const tlRef = useRef<Timeline | null>(null);
  const [connected, setConnected] = useState(false);
  const [live, setLive] = useState(true); // session live (vs detached)
  const [pid, setPid] = useState<number | null>(null);
  const [tmode, setTmode] = useState<"relative" | "current" | "utc">("relative");
  const [count, setCount] = useState(0);
  const [tracks, setTracks] = useState(0);
  const [gc, setGc] = useState(0);
  const [drag, setDrag] = useState<"pan" | "zoom">("zoom");
  const [helpOpen, setHelpOpen] = useState(false);
  const [slowOpen, setSlowOpen] = useState(false);
  const [slowCount, setSlowCount] = useState(0);
  const [slowList, setSlowList] = useState<ReturnType<Timeline["slowSpans"]>>([]);
  const [follow, setFollow] = useState(true);
  const [zoom, setZoom] = useState(1); // pxPerMs
  const [sort, setSort] = useState<SortMode>("recent1");
  const [headerH, setHeaderH] = useState(0);
  const [rowH, setRowH] = useState(18);
  const [hover, setHover] = useState<Hover | null>(null);
  const [selected, setSelected] = useState<Hover | null>(null);
  const [editor, setEditorState] = useState<string>(() => localStorage.getItem("gl.editor") || "vscode");
  const setEditor = (e: string) => { setEditorState(e); localStorage.setItem("gl.editor", e); };
  const [labels, setLabels] =
    useState<{ name: string; y: number; color: string; runMs: number }[]>([]);

  useEffect(() => {
    const tl = new Timeline(glRef.current!, overlayRef.current!);
    tlRef.current = tl;
    tl.onHover = setHover;
    tl.onSelect = setSelected;
    setHeaderH(tl.headerHeight());
    setRowH(tl.rowHeight());
    const poll = setInterval(() => {
      setCount(tl.count);
      setTracks(tl.nTracks);
      setFollow(tl.follow);
      setZoom(tl.pxPerMs);
      setSort(tl.sortMode);
      setGc(tl.gcCount());
      setSlowCount(tl.slowCount());
      setLabels(tl.trackLabels());
    }, 150);
    return () => clearInterval(poll);
  }, []);

  // Refresh the slow-log list only while the panel is open.
  useEffect(() => {
    if (!slowOpen) return;
    const tl = tlRef.current;
    if (!tl) return;
    const upd = () => setSlowList(tl.slowSpans(500));
    upd();
    const id = setInterval(upd, 500);
    return () => clearInterval(id);
  }, [slowOpen]);

  useEffect(() => {
    let ws: WebSocket;
    let stop = false;
    const connect = () => {
      const proto = location.protocol === "https:" ? "wss" : "ws";
      ws = new WebSocket(`${proto}://${location.host}/ws`);
      ws.onopen = () => setConnected(true);
      ws.onclose = () => {
        setConnected(false);
        if (!stop) setTimeout(connect, 1000);
      };
      ws.onmessage = (e) => {
        const msg: WsMsg = JSON.parse(e.data);
        if (msg.type === "meta") {
          setPid(msg.pid);
          setLive(msg.live);
          if (tlRef.current) tlRef.current.epochMs = msg.epochMs ?? NaN;
          if (!msg.live) freeze();
        } else if (msg.type === "status") {
          setLive(msg.live);
          if (!msg.live) freeze();
        } else if (msg.type === "gc") {
          tlRef.current?.addGc(msg.events);
        } else {
          if (msg.type === "snapshot") tlRef.current?.reset();
          tlRef.current?.addSlices(msg.slices);
        }
      };
    };
    connect();
    return () => { stop = true; ws?.close(); };
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
        <span className="title"><IconLane /> greenlane</span>
        <span className="stat">
          <span className={`dot ${connected && live ? "live" : "dead"}`} />
          {!connected ? "disconnected" : live ? "live" : "detached"}
        </span>
        {pid != null && <span className="stat">pid {pid}</span>}
        <span className="stat">{count.toLocaleString()} slices</span>
        <span className="stat">{tracks} greenlets</span>
        <span className="stat gc">{gc.toLocaleString()} GC</span>
        <label className="ctl">
          sort
          <select
            value={sort}
            onChange={(e) => {
              const m = e.target.value as SortMode;
              tlRef.current?.setSortMode(m);
              setSort(m);
            }}
          >
            <option value="recent1">activity (1s)</option>
            <option value="recent10">activity (10s)</option>
            <option value="recent60">activity (60s)</option>
            <option value="activity">activity (total)</option>
            <option value="ident">ident</option>
          </select>
        </label>
        <label className="ctl">
          time
          <select
            value={tmode}
            onChange={(e) => {
              const m = e.target.value as "relative" | "current" | "utc";
              tlRef.current?.setTimeMode(m);
              setTmode(m);
            }}
          >
            <option value="relative">relative</option>
            <option value="current">current</option>
            <option value="utc">utc</option>
          </select>
        </label>
        <div className="right">
          <button
            className="danger"
            onClick={() => fetch("/detach", { method: "POST" }).catch(() => {})}
            disabled={!connected || !live}
            title="stop instrumenting the target"
          >
            <IconDetach /> detach
          </button>
          <button
            className={follow ? "followon" : "followoff"}
            onClick={toggleFollow}
            title="follow live edge"
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
              style={{ top: l.y - headerH, height: rowH, lineHeight: `${rowH}px` }}
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
          spans={slowList}
          onPick={(idx) => tlRef.current?.revealSpan(idx)}
          onClose={() => setSlowOpen(false)}
        />
      )}
      <div className="bottombar">
        <button
          className={`slowtoggle${slowOpen ? " on" : ""}`}
          onClick={() => setSlowOpen((v) => !v)}
          title="slow spans (>20ms)"
        >
          slow log ({slowCount.toLocaleString()}) {slowOpen ? "▾" : "▸"}
        </button>
        <div className="bbright">
          <span className="seg" title="what dragging the timeline does">
            <button
              className={drag === "pan" ? "sel" : ""}
              onClick={() => { tlRef.current?.setDragMode("pan"); setDrag("pan"); }}
            ><IconHand /> pan</button>
            <button
              className={drag === "zoom" ? "sel" : ""}
              onClick={() => { tlRef.current?.setDragMode("zoom"); setDrag("zoom"); }}
            ><IconZoom /> zoom</button>
          </span>
          <div className="zoom">
            <input
              type="range" min={0} max={1} step={0.001}
              value={zoomToSlider(zoom)}
              onChange={(e) => {
                const px = sliderToZoom(Number(e.target.value));
                tlRef.current?.zoomTo(px);
                setZoom(px);
              }}
            />
            <span className="zval" title="time per pixel">{formatTime(1 / zoom)}/px</span>
            <button onClick={() => tlRef.current?.fit()} title="fit all to width">
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
            <div className="hrow"><b>scroll</b> scroll greenlet list</div>
            <div className="hrow"><b>⌘/ctrl + scroll</b> zoom in/out</div>
            <div className="hrow"><b>shift + scroll</b> pan time</div>
            <div className="hrow"><b>drag</b> {drag === "zoom" ? "zoom to selection" : "pan time"}</div>
            <div className="hrow"><b>ruler-drag / shift+drag</b> zoom to selection</div>
            <div className="hrow"><b>click span</b> open trace</div>
            <div className="hrow"><b>double-click</b> fit all</div>
          </div>
        )}
      </div>
    </div>
  );
}

// Bottom slow-log panel: warn/slow spans, newest first; click to jump.
function SlowLog({
  spans, onPick, onClose,
}: {
  spans: ReturnType<Timeline["slowSpans"]>;
  onPick: (idx: number) => void;
  onClose: () => void;
}) {
  const [by, setBy] = useState<"time" | "dur">("time");
  const [lvl, setLvl] = useState<"all" | "warn" | "red">("all");
  // spans arrive newest-first (by time); filter by level, then sort a copy.
  let rows = lvl === "all" ? spans : spans.filter((s) => (lvl === "red" ? s.level >= 2 : s.level === 1));
  if (by === "dur") rows = [...rows].sort((a, b) => b.durNs - a.durNs);
  return (
    <div className="slowlog">
      <div className="slowlog-head">
        <span>slow log · {rows.length} shown</span>
        <span className="segwrap">
          show
          <span className="seg">
            <button className={lvl === "all" ? "sel" : ""} onClick={() => setLvl("all")}>all</button>
            <button className={lvl === "warn" ? "sel" : ""} onClick={() => setLvl("warn")}>warn</button>
            <button className={lvl === "red" ? "sel" : ""} onClick={() => setLvl("red")}>red</button>
          </span>
        </span>
        <span className="segwrap">
          sort
          <span className="seg">
            <button className={by === "time" ? "sel" : ""} onClick={() => setBy("time")}>time</button>
            <button className={by === "dur" ? "sel" : ""} onClick={() => setBy("dur")}>duration</button>
          </span>
        </span>
        <span className="muted">click a row to jump</span>
        <button onClick={onClose} title="close"><IconClose /></button>
      </div>
      <div className="slowlog-body">
        {rows.length === 0 && <div className="slowrow muted">no spans over 20 ms yet</div>}
        {rows.map((s) => (
          <div key={s.idx} className="slowrow" onClick={() => onPick(s.idx)}>
            <span className="lvl" style={{ background: s.level >= 2 ? "#e8606b" : "#ebcb8b" }} />
            <span className="sdur" style={{ color: durColor(s.durNs / 1e6) }}>
              {formatTime(s.durNs / 1e6)}
            </span>
            <span className="snm">{s.name}</span>
            <span className="sfn">{s.func || "—"}</span>
            <span className="sat">+{formatTime(s.startNs / 1e6)}</span>
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
interface Frame { path: string; file: string; qual: string; line: string; lib: boolean }
function parseFrame(f: string): Frame {
  const parts = f.split(":");
  const line = parts.length >= 3 ? parts[parts.length - 1] : "";
  const qual = parts.length >= 3 ? parts.slice(1, -1).join(":") : parts[1] ?? "";
  const path = parts[0] ?? f;
  const file = path.split("/").pop() || path;
  const lib = path.includes("/gevent/") || path.includes("/greenlet");
  return { path, file, qual, line, lib };
}

// Inline SVG icons (no external assets — CSP-safe).
const svg = (children: any) => (
  <svg className="ico" width="13" height="13" viewBox="0 0 16 16" fill="none"
    stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
    {children}
  </svg>
);
// "Skip/stick to the latest" — a play triangle against an end bar.
const IconFollow = () => (
  <svg className="ico" width="13" height="13" viewBox="0 0 16 16" aria-hidden="true">
    <path d="M3 3.5L9.5 8 3 12.5z" fill="currentColor" />
    <rect x="11" y="3.5" width="2" height="9" rx="0.6" fill="currentColor" />
  </svg>
);
const IconFit = () => svg(<>
  <path d="M2 4v8" /><path d="M14 4v8" />
  <path d="M4 8h8" /><path d="M6 6L4 8l2 2" /><path d="M10 6l2 2-2 2" />
</>);
const IconZoom = () => svg(<><circle cx="7" cy="7" r="4.2" /><path d="M13.5 13.5l-3-3" /></>);
const IconClose = () => svg(<><path d="M4 4l8 8" /><path d="M12 4l-8 8" /></>);
const IconOpen = () => svg(<><path d="M6 3h7v7" /><path d="M13 3l-7 7" /><path d="M11 9v4H3V5h4" /></>);
const IconHelp = () => svg(<><circle cx="8" cy="8" r="6.5" /><path d="M6.2 6.2a1.8 1.8 0 1 1 2.4 1.7c-.6.3-.9.6-.9 1.3" /><circle cx="8" cy="11.6" r="0.1" /></>);
const IconHand = () => svg(<><path d="M6 7V4a1 1 0 0 1 2 0v3M8 7V3.4a1 1 0 0 1 2 0V7M10 7V4.4a1 1 0 0 1 2 0V10c0 2-1.5 3.5-3.7 3.5-1.4 0-2.5-.6-3.3-1.8L3.8 9a1 1 0 0 1 1.7-1l1 1.2" /></>);
const IconDetach = () => (
  <svg className="ico" width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
    <rect x="3.5" y="3.5" width="9" height="9" rx="1.6" fill="currentColor" />
  </svg>
);

// file:line → editor deep link.
function editorUrl(ed: string, path: string, line: string): string {
  const l = line || "1";
  if (ed === "pycharm") return `pycharm://open?file=${encodeURIComponent(path)}&line=${l}`;
  return `${ed}://file${path}:${l}`; // vscode, cursor, zed
}
const IconLane = () => (
  <svg className="ico" width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
    <rect x="1" y="2.5" width="8" height="2.6" rx="1.3" fill="#a3be8c" />
    <rect x="5" y="6.7" width="10" height="2.6" rx="1.3" fill="#88c0d0" />
    <rect x="2" y="10.9" width="6" height="2.6" rx="1.3" fill="#a3be8c" />
  </svg>
);

// Persistent right-side panel with the full, detailed call trace for a span:
// every captured frame (incl. library), full file paths, function + line.
function TracePanel({
  h, onClose, editor, onEditor,
}: {
  h: Hover; onClose: () => void; editor: string; onEditor: (e: string) => void;
}) {
  const frames = (h.stack ? h.stack.split(" <- ") : h.func ? [h.func] : []).map(parseFrame);
  const endNs = h.startNs + h.durNs;
  return (
    <div className="panel">
      <div className="panel-head">
        <span className="dot2" style={{ background: hueDot(h.name) }} />
        <span className="name">{h.name}</span>
        <button onClick={onClose} title="close"><IconClose /></button>
      </div>
      <div className="panel-body">
        <div className="row">
          <span className="k">duration</span>{" "}
          <span style={{ color: durColor(h.durNs / 1e6) }}>{formatTime(h.durNs / 1e6)}</span>
        </div>
        <div className="row"><span className="k">start</span> +{formatTime(h.startNs / 1e6)}</div>
        <div className="row"><span className="k">end</span> +{formatTime(endNs / 1e6)}</div>
        {h.task && <div className="row"><span className="k">task</span> {h.task}</div>}
        <div className="row"><span className="k">gid</span> 0x{h.gid.toString(16)}</div>
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
          {frames.length === 0 && <div className="frame">(no frames captured)</div>}
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
                <span className="openico"><IconOpen /></span>
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
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) & 1023;
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
      <div><span className="k">start</span> +{formatTime(h.startNs / 1e6)}</div>
      {h.task && <div><span className="k">task</span> {h.task}</div>}
      <div><span className="k">gid</span> 0x{h.gid.toString(16)}</div>
      <ShortStack h={h} />
    </div>
  );
}

// Short hover trace: app frames only (library hidden), basename, capped — the
// full detail (all frames, paths) lives in the click panel.
function ShortStack({ h }: { h: Hover }) {
  const all = (h.stack ? h.stack.split(" <- ") : h.func ? [h.func] : []).map(parseFrame);
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
          <span className="sloc"> {f.file}{f.line && `:${f.line}`}</span>
        </div>
      ))}
      {hidden > 0 && <div className="frame more">+{hidden} more · click for full trace</div>}
    </div>
  );
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
