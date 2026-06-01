// In-page instrumentation injected into the running app once, after load. It wraps
// the hot client functions to count how often they run, records audio-element events
// and footer mutations with timestamps, and captures console errors and long tasks.
// Flows read `window.__log` / `window.__counts` to assert on behavior and lag.
export const INSTRUMENT = `(() => {
  if (window.__instrumented) return "already";
  window.__instrumented = true;
  window.__t0 = performance.now();
  window.__log = [];
  window.__counts = {};
  window.__console = [];
  const stamp = () => Math.round(performance.now() - window.__t0);
  const push = (kind, d = {}) => { window.__log.push(Object.assign({ t: stamp(), kind }, d)); };
  window.__push = push;
  window.__reset = () => { window.__log = []; for (const k in window.__counts) window.__counts[k] = 0; window.__console = []; };

  const _err = console.error;
  console.error = (...a) => { window.__console.push(a.map(String).join(" ")); return _err.apply(console, a); };
  window.addEventListener("error", (e) => window.__console.push("uncaught: " + (e.message || e.error)));

  // Wrap a global function: bump a counter and log a marker each call.
  const wrap = (name, key, detail) => {
    const orig = window[name];
    if (typeof orig !== "function") return;
    window.__counts[key] = 0;
    window[name] = function (...args) {
      window.__counts[key]++;
      push(key, detail ? detail(args) : {});
      return orig.apply(this, args);
    };
  };
  // Heavy, should-be-rare-on-progress work:
  wrap("markActiveTrack", "markActiveTrack");
  wrap("renderQueue", "renderQueue");
  wrap("updateMediaSessionMetadata", "msMeta");
  // State application:
  wrap("updateFooterFromState", "fullState", (a) => ({ status: a[0] && a[0].status, now: a[0] && a[0].now_playing && a[0].now_playing.title || null }));
  wrap("applyProgressTick", "progressTick", (a) => ({ elapsed: a[0] && a[0].elapsed_seconds }));

  // driveBrowserAudio: capture whether it would reset the <audio> src (= a restart).
  // The audio element is a top-level const (not a window property), so reach it via
  // the DOM rather than window.
  const a = document.querySelector("#audio");
  const drive = window.driveBrowserAudio;
  window.__counts.drive = 0;
  window.__counts.srcReset = 0;
  if (typeof drive === "function" && a) {
    window.driveBrowserAudio = function (pb) {
      const now = pb.now_playing;
      window.__counts.drive++;
      const willReset = now && now.stream_url ? !a.src.endsWith(now.stream_url) : false;
      if (willReset) window.__counts.srcReset++;
      push("drive", { status: pb.status, now: (now && now.title) || null, srcWillReset: willReset });
      return drive.call(this, pb);
    };
    for (const type of ["loadstart", "play", "playing", "pause", "seeking", "seeked", "emptied", "ended"]) {
      a.addEventListener(type, () => push("audio:" + type, { ct: +a.currentTime.toFixed(2) }));
    }
  }

  const nt = document.querySelector("#now-title");
  if (nt) new MutationObserver(() => push("footerTitle", { text: nt.textContent }))
    .observe(nt, { childList: true, subtree: true, characterData: true });

  try {
    new PerformanceObserver((list) => { for (const e of list.getEntries()) push("longtask", { dur: Math.round(e.duration) }); })
      .observe({ entryTypes: ["longtask"] });
  } catch (e) { /* longtask unsupported */ }

  return "instrumented";
})()`;
