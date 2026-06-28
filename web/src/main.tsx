import {
  Component,
  StrictMode,
  useEffect,
  useRef,
  useState,
  type ErrorInfo,
  type ReactNode,
} from "react";
import { createRoot } from "react-dom/client";
import {
  Timeline,
  formatTime,
  formatTimePrecise,
  type ColorMode,
  type Hover,
  type SortMode,
} from "./timeline.ts";
import "./styles.css";
import { decodeWindow, formatBytes, formatRate } from "./wire.ts";

// Opt-in streaming/chunking diagnostics → browser console. Enable with
//   localStorage.setItem("gl.debug", "1")   (then reload),
// or live with  window.glDebug(true).  `window.glStats()` dumps a snapshot.
let GL_DEBUG =
  typeof localStorage !== "undefined" &&
  localStorage.getItem("gl.debug") === "1";
function dbg(event: string, data?: Record<string, unknown>) {
  if (GL_DEBUG) console.log(`%c[gl] ${event}`, "color:#88c0d0", data ?? "");
}
if (typeof window !== "undefined") {
  (window as unknown as { glDebug: (on?: boolean) => void }).glDebug = (
    on = true,
  ) => {
    GL_DEBUG = on;
    localStorage.setItem("gl.debug", on ? "1" : "0");
    console.log(`[gl] debug logging ${on ? "ON" : "off"}`);
  };
}

type Source = { file: string; bytes: number };
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
      totalExecutions: number;
      bytes: number;
      warnNs: number;
      blockNs: number;
      traces: boolean | null;
      traceMode: "off" | "slow" | "all" | null;
      retainedFromNs: number;
    }
  // Note: the viewport `window` reply is a BINARY columnar frame (see
  // decodeWindow), not a JSON message — so it isn't part of this union.
  // Live edge advance (so follow keeps moving and the header stays current).
  | {
      type: "head";
      spanNs: number;
      totalExecutions: number;
      bytes: number;
      retainedFromNs: number;
      lagMsPerSec: number | null;
    }
  | { type: "slowlog"; rows: SlowRow[]; total: number }
  | { type: "stats"; p50: number; p95: number; p99: number }
  // Lazy per-execution detail (the render-only window frame omits func/task/stack); the
  // viewer requests it on hover and matches the reply back by gid+startNs.
  | {
      type: "detail";
      gid: number;
      startNs: number;
      func: string;
      task: string;
      stack: string;
    }
  | { type: "status"; live: boolean };

function sortTitle(sort: SortMode): string {
  switch (sort) {
    case "recent1":
      return "Order greenlets by scheduler activity in the most recent 1 second.";
    case "recent10":
      return "Order greenlets by scheduler activity in the most recent 10 seconds.";
    case "recent60":
      return "Order greenlets by scheduler activity in the most recent 60 seconds.";
    case "activity":
      return "Order greenlets by total run time across the whole capture.";
    case "ident":
      return "Order greenlets by their stable runtime identity.";
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
  const [tmode, setTmode] = useState<"relative" | "current" | "utc">(
    "relative",
  );
  const [total, setTotal] = useState(0); // whole-capture execution count (server)
  const [tracks, setTracks] = useState(0);
  const [gc, setGc] = useState(0);
  const [rate, setRate] = useState(0);
  const [lag, setLag] = useState(0); // arrival lag (ms): real-time minus newest execution
  const [bufCount, setBufCount] = useState(0); // executions loaded in the viewer's window
  const [capped, setCapped] = useState(false); // window hit the execution cap
  const [warnMs, setWarnMs] = useState(20); // warn threshold (ms), from server
  const [blockMs, setBlockMs] = useState(50); // block threshold (ms), from server
  const [source, setSource] = useState<Source | null>(null);
  // Server-authoritative running totals, held in refs and surfaced via the poll
  // so they don't re-render per message.
  const [dataBytes, setDataBytes] = useState(0);
  const dataBytesRef = useRef(0);
  // Wire size of the current window frame — the "in view" data footprint shown
  // beside the buffered-execution count. Set on each binary window reply.
  const [viewBytes, setViewBytes] = useState(0);
  const viewBytesRef = useRef(0);
  const totalRef = useRef(0);
  // Live retention: the fixed capture origin and the (advancing) horizon below
  // which old rows have been evicted in a live-view-only session. When the
  // horizon passes the origin, data was dropped — surfaced in the header.
  const originRef = useRef(0);
  const [evictedFromNs, setEvictedFromNs] = useState(0);
  const wsRef = useRef<WebSocket | null>(null);
  const reqIdRef = useRef(0); // monotonic viewport request id (drop stale replies)
  const lastReqMs = useRef(0); // perf-clock of the last viewport request (debug)
  const [drag, setDrag] = useState<"pan" | "zoom">("zoom");
  const [helpOpen, setHelpOpen] = useState(false);
  // System panel: host/process/runtime details + live scheduler lag, fetched
  // from /info while the panel is open.
  const [sysOpen, setSysOpen] = useState(false);
  const [sysInfo, setSysInfo] = useState<Record<string, unknown> | null>(null);
  // Trace mode the capture used (--include-traces): "off" | "slow" | "all", or
  // null = unknown (recording). Drives the detail panel's per-execution copy.
  const [traceMode, setTraceMode] = useState<"off" | "slow" | "all" | null>(
    null,
  );
  const [slowOpen, setSlowOpen] = useState(false);
  // Slow-log panel height (px). Draggable via the handle on its top edge so it
  // can be expanded up to read more rows; clamped in SlowLog's resize handler.
  const [slowHeight, setSlowHeight] = useState(230);
  const [slowLevel, setSlowLevel] = useState<"all" | "warn" | "block">("all");
  const [slowSort, setSlowSort] = useState<"time" | "dur">("time");
  const [slowRows, setSlowRows] = useState<SlowRow[]>([]);
  const [slowTotal, setSlowTotal] = useState(0); // true count (uncapped) from DB
  // True while a *fresh* slow-log query (level/sort/open change) is in flight, so
  // the panel can mask the stale rows until the new ones land. Background polls
  // don't set it, so the steady-state refresh doesn't flicker.
  const [slowLoading, setSlowLoading] = useState(false);
  // Last slow-log query (level|sort) actually fetched. Used to mask the panel only
  // when the query changes — opening with background-fetched rows already present
  // shouldn't flash the "loading…" placeholder.
  const slowQueryRef = useRef("");
  const [follow, setFollow] = useState(true);
  const [zoom, setZoom] = useState(1); // pxPerMs
  const [sort, setSort] = useState<SortMode>("recent1");
  const [colorMode, setColorMode] = useState<ColorMode>("duration");
  const [headerH, setHeaderH] = useState(0);
  const [rowH, setRowH] = useState(18);
  const [hover, setHover] = useState<Hover | null>(null);
  const [selected, setSelected] = useState<Hover | null>(null);
  // Per-execution detail (func/task/stack) fetched lazily on hover — the window frame
  // is render-only. Cached by `gid:startNs` so a re-hover is instant and we never
  // re-request the same execution.
  const detailCacheRef = useRef<
    Map<string, { func: string; task: string; stack: string }>
  >(new Map());
  // Keys (`gid:startNs`) with a detail request in flight — so rapid re-hover of the
  // same execution (the effect fires on every mouse move) sends just one DB query.
  const detailPendingRef = useRef<Set<string>>(new Set());
  const [editor, setEditorState] = useState<string>(
    () => localStorage.getItem("gl.editor") || "vscode",
  );
  const setEditor = (e: string) => {
    setEditorState(e);
    localStorage.setItem("gl.editor", e);
  };
  // GC marker layer visibility (persisted). Data is always collected; this only
  // toggles the timeline's GC overlay.
  const [showGc, setShowGcState] = useState<boolean>(
    () => localStorage.getItem("gl.showGc") !== "0",
  );
  const setShowGc = (v: boolean) => {
    setShowGcState(v);
    localStorage.setItem("gl.showGc", v ? "1" : "0");
    tlRef.current?.setShowGc(v);
  };
  const [labels, setLabels] = useState<
    { name: string; y: number; color: string; runMs: number }[]
  >([]);

  useEffect(() => {
    const tl = new Timeline(glRef.current!, overlayRef.current!);
    tlRef.current = tl;
    tl.setShowGc(showGc); // apply persisted GC-overlay preference
    tl.onHover = setHover;
    tl.onSelect = setSelected;
    // The visible range changed → ask the server for exactly that window. Each
    // request carries a monotonic id; the server echoes it, and we apply only the
    // reply matching the latest request (drop superseded windows arriving mid-pan).
    tl.onViewport = (t0, t1, px, append) => {
      const ws = wsRef.current;
      if (ws && ws.readyState === WebSocket.OPEN) {
        reqIdRef.current += 1;
        dbg("request →", {
          req: reqIdRef.current,
          rangeMs: +((t1 - t0) / 1e6).toFixed(1),
          t0Ms: +(t0 / 1e6).toFixed(1),
          px,
          append: append !== undefined,
          follow: tl.follow,
          lastLoadMs: +tl.lastLoadMs.toFixed(1),
          dtSinceLastReqMs: +(performance.now() - lastReqMs.current).toFixed(0),
        });
        lastReqMs.current = performance.now();
        // `append` present → ask for just the new tail past the data frontiers
        // (`from`/`gcFrom`) to APPEND (live follow); the server replies with the same
        // frame shape plus an `append` flag.
        ws.send(
          JSON.stringify({
            type: "viewport",
            t0,
            t1,
            px,
            req: reqIdRef.current,
            ...(append ? { from: append.from, gcFrom: append.gcFrom } : {}),
          }),
        );
      }
    };
    setHeaderH(tl.headerHeight());
    setRowH(tl.rowHeight());
    // On-demand snapshot from the console: `glStats()`.
    (window as unknown as { glStats: () => unknown }).glStats = () =>
      tl.metrics();
    let lastHeartbeat = 0;
    const poll = setInterval(() => {
      setTotal(totalRef.current);
      setTracks(tl.nTracks);
      setFollow(tl.follow);
      setZoom(tl.pxPerMs);
      setSort(tl.sortMode);
      setGc(tl.gcCount());
      // Rate over the WHOLE capture (window count would understate it).
      setRate(
        tl.fullSpanMs > 0 ? totalRef.current / (tl.fullSpanMs / 1000) : 0,
      );
      setLag(tl.liveLagMs());
      setBufCount(tl.count); // executions the viewer holds for the current window
      setDataBytes(dataBytesRef.current);
      setViewBytes(viewBytesRef.current);
      setLabels(tl.trackLabels());
      // ~1s heartbeat of streaming state so growing lag / stalled buffers are
      // visible even when no window replies are arriving.
      if (GL_DEBUG && performance.now() - lastHeartbeat >= 1000) {
        lastHeartbeat = performance.now();
        const m = tl.metrics();
        dbg("heartbeat", {
          lagMs: +m.lagMs.toFixed(0),
          bufferExecutions: m.loadedExecutions,
          tracks: m.loadedTracks,
          loadedRangeMs: m.loadedMs.map((x) => +x.toFixed(0)),
          viewRangeMs: m.viewMs.map((x) => +x.toFixed(0)),
          inFlight: m.inFlight,
          follow: m.follow,
          capped: m.capped,
          lastLoadMs: +m.lastLoadMs.toFixed(1),
          totalExecutions: totalRef.current,
        });
      }
    }, 150);
    if (!GL_DEBUG) {
      console.info(
        "[gl] streaming debug off — run glDebug() (or localStorage.gl.debug=1) then reload to trace chunking/lag",
      );
    }
    return () => {
      clearInterval(poll);
      // Tear down the renderer (RAF loop + input listeners) so a remount — incl.
      // React StrictMode's dev double-mount — doesn't leave an old one running.
      tl.dispose();
      tlRef.current = null;
    };
  }, []);

  // The slow-log rows are a server-side DB query. Fetch once on mount and on any
  // open/level/sort change (keeps the count badge fresh on those transitions),
  // then poll once a second ONLY while the panel is open — no idle traffic.
  useEffect(() => {
    const send = () => {
      const ws = wsRef.current;
      if (ws && ws.readyState === WebSocket.OPEN) {
        // No artificial cap thanks to the DB: the badge shows the true total; the
        // limit only bounds how many rows we render in the (scrollable) list.
        ws.send(
          JSON.stringify({
            type: "slowlog",
            level: slowLevel,
            sort: slowSort,
            limit: 5000,
          }),
        );
      }
    };
    // Mask the panel only when the QUERY (level/sort) changed — those rows are
    // stale until the new tier lands. Merely opening the panel (with rows already
    // fetched in the background for the badge) must NOT flash "loading…", which
    // read as a one-frame blink. Interval polls reuse `send` without flagging load.
    const query = `${slowLevel}|${slowSort}`;
    const queryChanged = query !== slowQueryRef.current;
    slowQueryRef.current = query;
    if (slowOpen && queryChanged) setSlowLoading(true);
    send();
    // Keep the count badge live during capture even when the panel is closed
    // (that's the whole point of the badge). A static recording's count can't
    // change, so it needs only the single fetch above — no polling.
    if (!live) return;
    const id = setInterval(send, slowOpen ? 1000 : 2000);
    return () => clearInterval(id);
  }, [slowOpen, slowLevel, slowSort, live]);

  // Lazily fetch a execution's func/task/stack (the window frame omits them). Applies a
  // cached result instantly; otherwise asks the server, and the "detail" reply
  // merges back into hover/selected. No-ops once the execution already has its detail.
  const fetchDetail = (
    h: Hover | null,
    set: (u: (p: Hover | null) => Hover | null) => void,
  ) => {
    if (!h) return;
    const key = `${h.gid}:${h.startNs}`;
    const cached = detailCacheRef.current.get(key);
    if (cached) {
      if (h.func !== cached.func || h.stack !== cached.stack) {
        set((p) =>
          p && `${p.gid}:${p.startNs}` === key ? { ...p, ...cached } : p,
        );
      }
      return;
    }
    if (h.func) return; // already detailed
    if (detailPendingRef.current.has(key)) return; // request already in flight
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) {
      detailPendingRef.current.add(key);
      ws.send(
        JSON.stringify({
          type: "detail",
          gid: h.gid,
          startNs: h.startNs,
          durNs: h.durNs,
        }),
      );
      // Don't let a dropped/failed/no-match reply pin this key forever — clear it
      // after a grace period so a later hover retries. (The reply also clears it.)
      setTimeout(() => detailPendingRef.current.delete(key), 5000);
    }
  };
  useEffect(() => fetchDetail(hover, setHover), [hover]);
  useEffect(() => fetchDetail(selected, setSelected), [selected]);

  // While the System panel is open, poll /info (host/process/runtime facts +
  // live scheduler-lag) once a second. Closed → no traffic.
  useEffect(() => {
    if (!sysOpen) return;
    let stop = false;
    const load = () =>
      fetch("/info")
        .then((r) => r.json())
        .then((d) => {
          if (!stop) setSysInfo(d);
        })
        .catch(() => {});
    load();
    const id = setInterval(load, 1000);
    return () => {
      stop = true;
      clearInterval(id);
    };
  }, [sysOpen]);

  useEffect(() => {
    let ws: WebSocket;
    let stop = false;
    let reconnect: ReturnType<typeof setTimeout> | undefined;
    const connect = () => {
      if (stop) return; // unmounted before this (re)connect fired
      const proto = location.protocol === "https:" ? "wss" : "ws";
      ws = new WebSocket(`${proto}://${location.host}/ws`);
      ws.binaryType = "arraybuffer";
      wsRef.current = ws;
      ws.onopen = () => setConnected(true);
      ws.onclose = () => {
        setConnected(false);
        // Stop following the live edge while disconnected — there's no edge to
        // track, and the button is disabled below until we reconnect.
        if (tlRef.current) tlRef.current.follow = false;
        setFollow(false);
        // Any in-flight viewport request died with the socket — clear the guard
        // so the reconnected session can fetch immediately (don't wait out the
        // in-flight backstop).
        tlRef.current?.windowApplied();
        // Detail requests in flight got no reply; clear so hovers retry on reconnect.
        detailPendingRef.current.clear();
        if (!stop) reconnect = setTimeout(connect, 1000);
      };
      ws.onmessage = (e) => {
        const tl = tlRef.current;
        // The hot `window` reply arrives as a binary columnar frame; everything
        // else is small JSON text.
        if (e.data instanceof ArrayBuffer) {
          // A viewport reply landed → clear the in-flight guard so follow can
          // issue the next request (whatever the frame's fate below).
          tl?.windowApplied();
          const t0Perf = performance.now();
          let h;
          try {
            h = decodeWindow(e.data);
          } catch (err) {
            console.error("dropping malformed window frame:", err);
            return;
          }
          const decodeMs = performance.now() - t0Perf;
          // Drop superseded replies: only the reply to the most recent viewport
          // request is the current view. (req 0 = legacy/no-id → always apply.)
          if (h.req !== 0 && h.req !== reqIdRef.current) {
            dbg("reply ✗ dropped (stale)", {
              req: h.req,
              latest: reqIdRef.current,
              executions: h.startMs.length,
              frameBytes: (e.data as ArrayBuffer).byteLength,
            });
            return;
          }
          dbg("reply ←", {
            req: h.req,
            executions: h.startMs.length,
            tracks: h.tracks.length,
            frameKB: +((e.data as ArrayBuffer).byteLength / 1024).toFixed(1),
            capped: h.capped,
            decodeMs: +decodeMs.toFixed(1),
          });
          dataBytesRef.current = h.bytes;
          viewBytesRef.current = (e.data as ArrayBuffer).byteLength;
          totalRef.current = h.total;
          setCapped(h.capped);
          setEvictedFromNs(
            h.retainedFromNs > originRef.current ? h.retainedFromNs : 0,
          );
          if (tl) tl.retentionActive = h.retainedFromNs > 0;
          // C1: track whether the timeline is start-sorted; once it isn't (concurrent
          // hubs → out-of-order spans) the viewer stops using the append fast path.
          if (tl) tl.lastSorted = h.sorted;
          tl?.setSpan(h.spanNs);
          if (tl && h.append) {
            // Live-follow tail: append the new rows to the existing buffer (the
            // viewer requested only `[from, t1]`). A capped tail is pathological (a
            // single tick produced > the cap of new rows) and would leave a gap to
            // t1 — don't append a gappy tail; force a full re-centered load next
            // frame by marking the window capped.
            if (h.capped) {
              tl.lastWindowCapped = true;
            } else {
              tl.loadWindowAppend(
                h.t0,
                h.t1,
                h.maxStart,
                h.tracks,
                h.gc,
                h.startMs,
                h.durMs,
                h.trackIdx,
              );
            }
          } else if (tl) {
            tl.loadWindowColumnar(
              h.t0,
              h.maxStart,
              h.tracks,
              h.gc,
              h.startMs,
              h.durMs,
              h.trackIdx,
            );
            // After load record the range we actually hold (the real data bounds
            // when capped, the requested window otherwise — see setLoadedRange).
            tl.setLoadedRange(
              h.t0,
              h.t1,
              h.minStart,
              h.maxEnd,
              h.capped,
              h.startMs.length,
            );
          }
          if (GL_DEBUG && tl) {
            const m = tl.metrics();
            dbg("applied ✓", {
              req: h.req,
              ingestMs: +m.lastLoadMs.toFixed(1),
              totalApplyMs: +(performance.now() - t0Perf).toFixed(1),
              bufferExecutions: m.loadedExecutions,
              loadedRangeMs: m.loadedMs.map((x) => +x.toFixed(0)),
              viewRangeMs: m.viewMs.map((x) => +x.toFixed(0)),
              lagMs: +m.lagMs.toFixed(0),
              capped: m.capped,
            });
          }
          return;
        }
        const msg: WsMsg = JSON.parse(e.data);
        switch (msg.type) {
          case "meta": {
            // Previous session's origin (0 on first connect) — a change means we
            // reconnected to a *restarted* server with a new capture (C2).
            const prevOrigin = originRef.current;
            setLive(msg.live);
            setSource(msg.source);
            setTraceMode(msg.traceMode ?? null);
            dataBytesRef.current = msg.bytes;
            totalRef.current = msg.totalExecutions;
            // Span-duration thresholds (configurable server-side): drive execution
            // colors, slow-log labels, and durColor.
            const wMs = msg.warnNs / 1e6;
            const bMs = msg.blockNs / 1e6;
            gWarnMs = wMs;
            gBlockMs = bMs;
            setWarnMs(wMs);
            setBlockMs(bMs);
            originRef.current = msg.originNs;
            setEvictedFromNs(
              msg.retainedFromNs > msg.originNs ? msg.retainedFromNs : 0,
            );
            if (tl) {
              // Fresh (re)connection: no viewport request is outstanding on this
              // socket, so re-arm follow immediately rather than waiting out the
              // in-flight timeout.
              tl.windowApplied();
              tl.retentionActive = msg.retainedFromNs > 0;
              tl.epochMs = msg.epochMs ?? NaN;
              tl.live = msg.live; // drives the wall-clock follow edge + lag
              tl.setOrigin(msg.originNs);
              // C2: reconnected to a different capture (origin moved) → drop the stale
              // buffer + cursors so the first load is a clean full window, not an
              // append against data from the old origin. Same-origin reconnects keep
              // their buffer (no greenlet reshuffle).
              if (prevOrigin !== 0 && msg.originNs !== prevOrigin) tl.reset();
              tl.setSpan(msg.spanNs);
              tl.setThresholds(wMs, bMs);
              // A recording is static: stop following and fit the whole span so
              // the first window covers it. Live keeps following the edge.
              if (!msg.live) {
                tl.follow = false;
                setFollow(false);
                tl.fit();
              }
            }
            break;
          }
          case "head":
            dataBytesRef.current = msg.bytes;
            totalRef.current = msg.totalExecutions;
            tl?.setSpan(msg.spanNs);
            tl?.addLag(msg.spanNs, msg.lagMsPerSec); // R13: scheduler-lag band
            setEvictedFromNs(
              msg.retainedFromNs > originRef.current ? msg.retainedFromNs : 0,
            );
            if (tl) tl.retentionActive = msg.retainedFromNs > 0;
            break;
          case "slowlog":
            setSlowRows(msg.rows);
            setSlowTotal(msg.total);
            setSlowLoading(false);
            break;
          case "detail": {
            // Lazy per-execution detail: cache it and merge into hover/selected if they
            // still point at the same execution (gid+startNs).
            const key = `${msg.gid}:${msg.startNs}`;
            const d = { func: msg.func, task: msg.task, stack: msg.stack };
            detailCacheRef.current.set(key, d);
            detailPendingRef.current.delete(key);
            const merge = (p: Hover | null) =>
              p && p.gid === msg.gid && p.startNs === msg.startNs
                ? { ...p, ...d }
                : p;
            setHover(merge);
            setSelected(merge);
            break;
          }
          case "status":
            setLive(msg.live);
            if (tl) tl.live = msg.live;
            if (!msg.live) freeze();
            break;
        }
      };
    };
    connect();
    return () => {
      stop = true;
      clearTimeout(reconnect); // cancel a pending reconnect so it can't open a socket post-unmount
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
          title="greenlane: live timeline profiler for gevent scheduler activity"
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
        <span
          className="stat"
          title={`${total.toLocaleString()} executions (${formatBytes(dataBytes)}) collected across the whole capture; ${bufCount.toLocaleString()} (${formatBytes(viewBytes)}) held in the viewer for the visible window (plus a small pan margin). The buffered set is bounded — only the window is fetched, not the whole capture — so it stays flat as the capture grows; ⚠ capped means it hit the render cap, so zoom in for full detail. Each execution is one continuous task or greenlet run.`}
        >
          {total.toLocaleString()} executions ({formatBytes(dataBytes)})
          <span className="sub">
            {" "}
            · in view {bufCount.toLocaleString()} ({formatBytes(viewBytes)})
          </span>
        </span>
        <span
          className="stat"
          title="Mean scheduler events per second over the captured time span."
        >
          {formatRate(rate)}
        </span>
        <span
          className="stat"
          title="Number of greenlets discovered in the trace."
        >
          {tracks} greenlets
        </span>
        <span
          className="stat gc"
          title="Garbage-collection pauses captured as global timeline markers. Toggle the markers with the GC button in the bottom bar."
        >
          {gc.toLocaleString()} GC
        </span>
        {live && (
          <span
            className="stat"
            title="Arrival lag: how far the newest rendered execution trails real time (capture + transport delay). The live edge moves on the wall clock; executions fill in behind it."
            style={lag > 1000 ? { color: "var(--ac-warn)" } : undefined}
          >
            lag {formatTime(lag)}
          </span>
        )}
        {capped && (
          <span
            className="stat"
            title="The visible range has more executions than the render cap; zoom in to see them all."
            style={{ color: "var(--ac-warn)" }}
          >
            ⚠ capped
          </span>
        )}
        {evictedFromNs > 0 && (
          <span
            className="stat"
            title={`Live retention: this session caps how much it keeps in memory, so executions older than ${formatTime((evictedFromNs - originRef.current) / 1e6)} into the capture have been evicted. The slow log, percentiles, and counts describe only retained data. Record (omit --serve, or add --out) to keep everything.`}
            style={{ color: "var(--ac-warn)" }}
          >
            ⚠ old data evicted
          </span>
        )}
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
            className={follow ? "followon" : ""}
            onClick={toggleFollow}
            disabled={!connected || !live}
            aria-pressed={follow}
            title={
              !connected
                ? "Disconnected — nothing to follow."
                : follow
                  ? "Following the live edge. Click to pause and hold the current viewport."
                  : "Paused. Click to follow the live edge as new data arrives."
            }
          >
            {follow ? (
              <>
                <IconPause /> pause
              </>
            ) : (
              <>
                <IconFollow /> follow
              </>
            )}
          </button>
        </div>
      </div>
      {sysOpen && <SysPanel info={sysInfo} onClose={() => setSysOpen(false)} />}
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
              title={`${l.name} — ran ${formatTime(l.runMs)} in the visible range (its scheduler activity; greenlets are ordered by this).`}
            >
              <span className="dot2" style={{ background: l.color }} />
              <span className="nm">{l.name}</span>
              <span
                className="rt"
                title="Run time of this greenlet within the visible range — how long it held the scheduler. This is the 'activity' the sort uses."
              >
                {formatTime(l.runMs)}
              </span>
            </div>
          ))}
        </div>
        {hover && <Tooltip h={hover} />}
        {selected && (
          <TracePanel
            h={selected}
            traceMode={traceMode}
            onClose={() => {
              setSelected(null);
              tlRef.current?.clearSelection();
            }}
            editor={editor}
            onEditor={setEditor}
          />
        )}
      </div>
      {slowOpen && (
        <SlowLog
          rows={slowRows}
          total={slowTotal}
          loading={slowLoading}
          level={slowLevel}
          sort={slowSort}
          warnMs={warnMs}
          blockMs={blockMs}
          height={slowHeight}
          onResize={setSlowHeight}
          onLevel={setSlowLevel}
          onSort={setSlowSort}
          onPick={(s) => tlRef.current?.revealSpanAt(s.start, s.dur, s.gid)}
          onClose={() => setSlowOpen(false)}
        />
      )}
      <div className="bottombar">
        <div className="bbleft">
          <button
            className={`slowtoggle${slowOpen ? " on" : ""}`}
            onClick={() => setSlowOpen((v) => !v)}
            aria-pressed={slowOpen}
            title={`Spans that ran long enough to stall the scheduler (≥${warnMs}ms), queried from the database.`}
          >
            <IconSlow /> slow log ({slowTotal.toLocaleString()}){" "}
            {slowOpen ? "▾" : "▸"}
          </button>
          <span className="bbsep" aria-hidden="true" />
          <label className="ctl ctlsort" title={sortTitle(sort)}>
            <IconSort />
            <select
              value={sort}
              aria-label="sort greenlets"
              title={sortTitle(sort)}
              onChange={(e) => {
                const m = e.target.value as SortMode;
                tlRef.current?.setSortMode(m);
                setSort(m);
              }}
            >
              <option
                value="recent1"
                title="Put greenlets with the most run time in the latest 1 second first."
              >
                1s
              </option>
              <option
                value="recent10"
                title="Put greenlets with the most run time in the latest 10 seconds first."
              >
                10s
              </option>
              <option
                value="recent60"
                title="Put greenlets with the most run time in the latest 60 seconds first."
              >
                60s
              </option>
              <option
                value="activity"
                title="Put greenlets with the highest total run time first."
              >
                total
              </option>
              <option value="ident" title="Use stable runtime identity order.">
                ident
              </option>
            </select>
          </label>
          <label
            className="ctl"
            title="Greenlet fill color: by greenlet identity, by execution duration (blue < warn, yellow < block, red beyond), or as a heatmap (continuous cool→hot gradient by run length). Hub stays green."
          >
            <IconColor />
            <select
              value={colorMode}
              aria-label="span color mode"
              onChange={(e) => {
                const m = e.target.value as ColorMode;
                tlRef.current?.setColorMode(m);
                setColorMode(m);
              }}
            >
              <option value="ident" title="A stable color per greenlet.">
                identity
              </option>
              <option
                value="duration"
                title="Color executions by how long they ran: blue < warn, yellow < block, red beyond. Hub stays green."
              >
                duration
              </option>
              <option
                value="heatmap"
                title="Color executions on a continuous cool→hot gradient by run length (log-scaled): dark/indigo for short runs, yellow for the longest. Hub stays green."
              >
                heatmap
              </option>
            </select>
          </label>
          <label className="ctl" title={timeModeTitle(tmode)}>
            <IconClock />
            <select
              value={tmode}
              aria-label="time format"
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
          <button
            className={`gcbtn${showGc ? " on" : ""}`}
            onClick={() => setShowGc(!showGc)}
            title={
              showGc
                ? "GC pause markers are shown on the timeline. Click to hide them (they stay captured and counted either way)."
                : "GC pause markers are hidden. Click to show them on the timeline."
            }
            aria-pressed={showGc}
          >
            {showGc ? <IconEye /> : <IconEyeOff />} GC
          </button>
        </div>
        <div className="bbright">
          <span className="seg" title="what dragging the timeline does">
            <button
              className={drag === "pan" ? "sel" : ""}
              onClick={() => {
                tlRef.current?.setDragMode("pan");
                setDrag("pan");
              }}
              aria-pressed={drag === "pan"}
              title="Drag to scroll the timeline left/right through time."
            >
              <IconHand /> pan
            </button>
            <button
              className={drag === "zoom" ? "sel" : ""}
              onClick={() => {
                tlRef.current?.setDragMode("zoom");
                setDrag("zoom");
              }}
              aria-pressed={drag === "zoom"}
              title="Drag to select a time range and zoom into it."
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
              title="Zoom the time scale: left = zoomed out (more time on screen), right = zoomed in. Also ⌘/Ctrl-scroll on the timeline."
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
          <span className="bbsep" aria-hidden="true" />
          <button
            className={`sysbtn${sysOpen ? " on" : ""}`}
            onClick={() => setSysOpen((v) => !v)}
            aria-pressed={sysOpen}
            title="System: host, process, interpreter, and kernel scheduler-lag details."
          >
            <IconInfo /> system
          </button>
          <button
            className={`help${helpOpen ? " on" : ""}`}
            onClick={() => setHelpOpen((v) => !v)}
            aria-pressed={helpOpen}
            title="Keyboard and mouse controls."
          >
            <IconHelp /> help
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
              <b>click execution</b> open trace
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

// System panel: host / process / interpreter facts and live kernel scheduler
// lag, polled from /info while open. Scheduler lag (run-queue wait + cgroup
// throttling) is Linux-only and live-only; recordings show what they have.
function SysPanel({
  info,
  onClose,
}: {
  info: Record<string, any> | null;
  onClose: () => void;
}) {
  const py = info?.python as any;
  const k = info?.kernel as any;
  const pr = info?.process as any;
  const lag = info?.lag as any;
  const live = info?.live as boolean | undefined;

  const Row = ({
    k: key,
    v,
  }: {
    k: string;
    v: string | number | null | undefined;
  }) =>
    v === null || v === undefined || v === "" ? null : (
      <div className="sprow">
        <span className="spk">{key}</span>
        <span className="spv">{v}</span>
      </div>
    );

  return (
    <div className="syspanel">
      <div className="sphead">
        <span>system</span>
        <button onClick={onClose} title="close" aria-label="close">
          <IconClose />
        </button>
      </div>
      <div className="spbody">
        {!info ? (
          <div className="spnote">loading…</div>
        ) : (
          <>
            <section>
              <h4>runtime</h4>
              <Row k="kind" v={py?.runtime} />
              <Row
                k="python"
                v={py ? `${py.python} (${py.implementation})` : null}
              />
              <Row k="gevent" v={py?.gevent} />
              <Row k="executable" v={py?.executable} />
              <Row k="pid" v={info.pid as number} />
              <Row k="thread" v={py?.thread} />
              <Row k="tid" v={info.tid as number} />
            </section>
            <section>
              <h4>process</h4>
              <Row k="state" v={pr?.state} />
              <Row k="threads" v={pr?.threads} />
              <Row
                k="rss"
                v={pr?.rssKb ? formatBytes(pr.rssKb * 1024) : null}
              />
              <Row
                k="peak"
                v={pr?.vmPeakKb ? formatBytes(pr.vmPeakKb * 1024) : null}
              />
              <Row k="invol. ctxt switches" v={pr?.involuntaryCtxt} />
            </section>
            <section>
              <h4>host / kernel</h4>
              <Row
                k="os"
                v={k ? `${k.os ?? ""} ${k.release ?? ""}`.trim() : null}
              />
              <Row k="arch" v={k?.machine} />
              <Row k="cpus" v={k?.cpus} />
              <Row
                k="cpu quota"
                v={
                  k == null
                    ? null
                    : k.cgroupQuotaCores != null
                      ? `${k.cgroupQuotaCores} cores`
                      : "unlimited"
                }
              />
            </section>
            <section>
              <h4>scheduler lag</h4>
              {lag?.available ? (
                <>
                  <Row
                    k="runqueue wait"
                    v={`${(lag.runqRateMsPerSec ?? 0).toFixed(1)} ms/s`}
                  />
                  <Row k="total wait" v={formatTime(lag.runqWaitMs ?? 0)} />
                  <Row k="on-cpu" v={formatTime(lag.onCpuMs ?? 0)} />
                  {lag.throttle && (
                    <>
                      <Row
                        k="throttled"
                        v={`${lag.throttle.throttled} / ${lag.throttle.periods} periods`}
                      />
                      <Row
                        k="throttled time"
                        v={formatTime(lag.throttle.throttledMs ?? 0)}
                      />
                    </>
                  )}
                  {lag.psiSomeAvg10 != null && (
                    <Row k="cpu pressure 10s" v={`${lag.psiSomeAvg10}%`} />
                  )}
                </>
              ) : (
                <div className="spnote">
                  {live === false
                    ? "recording — live scheduler metrics unavailable"
                    : "unavailable (needs Linux schedstat)"}
                </div>
              )}
            </section>
          </>
        )}
      </div>
    </div>
  );
}

// Bottom slow-log panel. Rows are queried from the database (the level/sort
// controls re-issue the query upstream); click a row to seek the timeline to it.
function SlowLog({
  rows,
  total,
  loading,
  level,
  sort,
  warnMs,
  blockMs,
  height,
  onResize,
  onLevel,
  onSort,
  onPick,
  onClose,
}: {
  rows: SlowRow[];
  total: number;
  loading: boolean;
  level: "all" | "warn" | "block";
  sort: "time" | "dur";
  warnMs: number;
  blockMs: number;
  height: number;
  onResize: (h: number) => void;
  onLevel: (l: "all" | "warn" | "block") => void;
  onSort: (s: "time" | "dur") => void;
  onPick: (row: SlowRow) => void;
  onClose: () => void;
}) {
  // Drag the top edge to resize. Dragging up (negative dy) grows the panel;
  // clamp to a readable minimum and ~80% of the viewport so it can't swallow
  // the timeline entirely or collapse past one row.
  function startResize(e: React.PointerEvent) {
    e.preventDefault();
    const startY = e.clientY;
    const startH = height;
    const max = Math.max(160, window.innerHeight * 0.8);
    const move = (ev: PointerEvent) => {
      const h = startH + (startY - ev.clientY);
      onResize(Math.min(max, Math.max(120, h)));
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }
  // The DB filters by tier server-side (all = warn+block, warn = warn-band only,
  // block = block-band only) BEFORE the display limit, so `rows`/`total` are
  // already the requested tier — no client-side re-filter (which would miss
  // warn-tier rows when blocks fill the limited page).
  const shown = rows;
  return (
    <div className="slowlog" style={{ height }}>
      <div
        className="slowlog-resize"
        onPointerDown={startResize}
        title="Drag to resize the slow log"
      />
      <div className="slowlog-head">
        <span title="Executions shown here vs. the total matching in the database.">
          slow log · {shown.length.toLocaleString()} shown
          {total > rows.length ? ` of ${total.toLocaleString()}` : ""}
        </span>
        <span className="segwrap">
          show
          <span className="seg">
            <button
              className={level === "all" ? "sel" : ""}
              onClick={() => onLevel("all")}
              title={`All executions ≥ ${warnMs}ms.`}
            >
              all
            </button>
            <button
              className={level === "warn" ? "sel" : ""}
              onClick={() => onLevel("warn")}
              title={`Warning tier: ${warnMs}–${blockMs}ms — getting long.`}
            >
              warn
            </button>
            <button
              className={level === "block" ? "sel" : ""}
              onClick={() => onLevel("block")}
              title={`Blocking tier: ≥ ${blockMs}ms — long enough to stall the scheduler.`}
            >
              block
            </button>
          </span>
        </span>
        <span className="segwrap">
          sort
          <span className="seg">
            <button
              className={sort === "time" ? "sel" : ""}
              onClick={() => onSort("time")}
              title="Sort the slow log by when each execution occurred (chronological)."
            >
              time
            </button>
            <button
              className={sort === "dur" ? "sel" : ""}
              onClick={() => onSort("dur")}
              title="Sort the slow log by execution duration, longest first."
            >
              duration
            </button>
          </span>
        </span>
        <span className="muted">click a row to jump</span>
        <button onClick={onClose} title="close" aria-label="close">
          <IconClose />
        </button>
      </div>
      <div className="slowlog-body">
        {loading ? (
          // Mask the stale rows while a fresh query (level/sort switch) runs — on a
          // large log the swap is otherwise a visible lag with old rows lingering.
          <div className="slowrow muted slowloading">loading…</div>
        ) : (
          <>
            {shown.length === 0 && (
              <div className="slowrow muted">
                {level === "block"
                  ? `no executions ≥ ${blockMs} ms yet`
                  : level === "warn"
                    ? `no executions in the ${warnMs}–${blockMs} ms range yet`
                    : `no executions ≥ ${warnMs} ms yet`}
              </div>
            )}
            {shown.map((s, i) => (
              <div key={i} className="slowrow" onClick={() => onPick(s)}>
                <span
                  className="lvl"
                  style={{
                    background:
                      s.level >= 2 ? "var(--ac-block)" : "var(--ac-warn)",
                  }}
                />
                <span className="sdur" style={{ color: durColor(s.dur / 1e6) }}>
                  {formatTimePrecise(s.dur / 1e6)}
                </span>
                <span className="snm">{s.name}</span>
                <span className="sfn">{s.func || "—"}</span>
                <span className="sat">+{formatTimePrecise(s.start / 1e6)}</span>
              </div>
            ))}
          </>
        )}
      </div>
    </div>
  );
}

// Warn/block thresholds (ms), updated from the server `meta`. Module-level so the
// pure durColor helper (used in several places) reflects config without prop drilling.
let gWarnMs = 20;
let gBlockMs = 50;

// Duration → highlight color: yellow ≥ warn, red ≥ block.
function durColor(ms: number): string | undefined {
  if (ms >= gBlockMs) return "var(--ac-block)";
  if (ms >= gWarnMs) return "var(--ac-warn)";
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
const IconPause = () => (
  <svg
    className="ico"
    width="13"
    height="13"
    viewBox="0 0 16 16"
    aria-hidden="true"
  >
    <rect x="4" y="3" width="3" height="10" rx="0.6" fill="currentColor" />
    <rect x="9" y="3" width="3" height="10" rx="0.6" fill="currentColor" />
  </svg>
);
// Slow log: an hourglass (two triangles tip-to-tip) — long-running executions.
const IconSlow = () =>
  svg(
    <>
      <path d="M4 2.5h8l-4 5.5z" />
      <path d="M4 13.5h8l-4-5.5z" />
    </>,
  );
// Bottom-bar control glyphs: sort (descending bars), color (paint droplet),
// time (clock) — replace the text labels to keep the bar compact.
const IconSort = () =>
  svg(
    <>
      <path d="M3 4.5h10" />
      <path d="M3 8h6.5" />
      <path d="M3 11.5h3.5" />
    </>,
  );
const IconColor = () =>
  svg(
    <>
      <path d="M8 2.5C5.5 6 4 8 4 10a4 4 0 0 0 8 0c0-2-1.5-4-4-7.5z" />
    </>,
  );
const IconClock = () =>
  svg(
    <>
      <circle cx="8" cy="8" r="6" />
      <path d="M8 4.6V8l2.4 1.5" />
    </>,
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
const IconInfo = () =>
  svg(
    <>
      <circle cx="8" cy="8" r="6.5" />
      <path d="M8 7.3v3.4" />
      <circle cx="8" cy="5" r="0.1" />
    </>,
  );
// Eye (layer visible) / eye-off (layer hidden) — GC marker toggle state.
const IconEye = () =>
  svg(
    <>
      <path d="M1.5 8s2.5-4.5 6.5-4.5S14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8z" />
      <circle cx="8" cy="8" r="2" />
    </>,
  );
const IconEyeOff = () =>
  svg(
    <>
      <path d="M6.3 3.7A6.6 6.6 0 0 1 8 3.5c4 0 6.5 4.5 6.5 4.5a12 12 0 0 1-2 2.5" />
      <path d="M3.5 4.8A11.5 11.5 0 0 0 1.5 8S4 12.5 8 12.5c.9 0 1.7-.2 2.4-.5" />
      <path d="M2 2l12 12" />
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

// Persistent right-side panel with the full, detailed call trace for a execution:
// every captured frame (incl. library), full file paths, function + line.
function TracePanel({
  h,
  traceMode,
  onClose,
  editor,
  onEditor,
}: {
  h: Hover;
  traceMode: "off" | "slow" | "all" | null;
  onClose: () => void;
  editor: string;
  onEditor: (e: string) => void;
}) {
  const frames = (h.stack ? h.stack.split(" <- ") : h.func ? [h.func] : []).map(
    parseFrame,
  );
  // Whether THIS execution carries a full captured stack (vs only its cheap leaf
  // label). Stacks are gated per execution by the trace mode, so this is per-execution.
  const hasStack = !!h.stack;
  // Explain a missing full stack: traces off, or `slow` mode and this execution wasn't
  // slow enough. (For `all`/recordings a missing stack is just "none captured".)
  const noStackHint =
    !hasStack && traceMode === "off"
      ? "off"
      : !hasStack && traceMode === "slow"
        ? "slow"
        : null;
  const endNs = h.startNs + h.durNs;
  return (
    <div className="panel">
      <div className="panel-head">
        <span className="dot2" style={{ background: hueDot(h.name) }} />
        <span className="name">{h.name}</span>
        <button onClick={onClose} title="close" aria-label="close">
          <IconClose />
        </button>
      </div>
      <div className="panel-body">
        <div className="row">
          <span className="k">duration</span>{" "}
          <span style={{ color: durColor(h.durNs / 1e6) }}>
            {formatTimePrecise(h.durNs / 1e6)}
          </span>
        </div>
        <div className="row">
          <span className="k">start</span> +{formatTimePrecise(h.startNs / 1e6)}
        </div>
        <div className="row">
          <span className="k">end</span> +{formatTimePrecise(endNs / 1e6)}
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
          <span>
            {hasStack
              ? `call trace (${frames.length} frames · leaf → root)`
              : "leaf function only"}
          </span>
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
        {noStackHint === "off" && (
          <div className="trace-off">
            <p>
              Trace capture is <code>off</code> — only each execution's leaf
              function is recorded. Re-attach with a trace mode to capture the
              full leaf → root stack:
            </p>
            <pre>
              greenlane attach &lt;PID&gt; --include-traces slow --serve
            </pre>
            <p className="trace-off-warn">
              <code>slow</code> (the default) walks the stack only for
              executions over the warn threshold — cheap enough to leave on;{" "}
              <code>all</code> captures every execution.
            </p>
          </div>
        )}
        {noStackHint === "slow" && (
          <div className="trace-off">
            <p>
              This execution ran under the warn threshold, so its full stack
              wasn't captured (<code>--include-traces slow</code> walks only
              slow executions). Its leaf function is shown above.
            </p>
            <p className="trace-off-warn">
              Re-attach with <code>--include-traces all</code> to capture every
              execution's stack, or lower <code>--warn-ms</code> to widen what
              counts as slow.
            </p>
          </div>
        )}
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

// Catches any render-time error in the tree and shows a readable error page
// instead of React unmounting everything (which left the viewer a black screen).
// The error + component stack also go to the console for debugging.
class ErrorBoundary extends Component<
  { children: ReactNode },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("[gl] viewer crashed:", error, info.componentStack);
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div className="errorpage">
        <div className="errorbox">
          <h1>greenlane viewer crashed</h1>
          <p>
            A rendering error took down the UI. The details are below and in the
            browser console.
          </p>
          <pre className="errmsg">{error.message || String(error)}</pre>
          {error.stack && <pre className="errstack">{error.stack}</pre>}
          <button onClick={() => location.reload()}>Reload</button>
        </div>
      </div>
    );
  }
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>,
);
