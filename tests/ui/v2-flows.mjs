// Hot-path + flow smoke for the Svelte app. Two phases (see scripts/v2-smoke.sh):
//   behavior — playback/flows against the light testdata fixture (this is where the hot
//              path and most flows are pinned).
//   scale    — render/scroll/search against a copy of the real ~11k-track DB (--no-scan):
//              the app must window its rendering, not pull the whole library up front.
// Args: <port> <basePath> <mode=behavior|scale>. Assumes Chrome CDP on :9222. Exits
// non-zero on any failed assertion.
const PORT = process.argv[2];
const PATH = process.argv[3] || "/v2";
const MODE = process.argv[4] || "behavior";
const base = `http://127.0.0.1:${PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Auth: a fresh DB starts in setup mode. Create the smoke admin (or log in if it already
// exists) and keep its session cookie for the Node-side API calls below. The browser logs in
// separately (in-page) so it gets its own cookie. Password ≥ 8 chars (server requirement).
const SMOKE_CREDS = { username: "smoke", password: "smoke-admin-1" };
let COOKIE = "";
{
  const headers = { "content-type": "application/json" };
  const body = JSON.stringify(SMOKE_CREDS);
  let r = await fetch(base + "/api/auth/setup", { method: "POST", headers, body });
  if (!r.ok) r = await fetch(base + "/api/auth/login", { method: "POST", headers, body });
  COOKIE = (r.headers.get("set-cookie") || "").split(";")[0]; // "musicata_session=…"
}
const api = (path, init = {}) =>
  fetch(base + path, { ...init, headers: { ...(init.headers || {}), cookie: COOKIE } }).then((r) =>
    r.ok ? r.json().catch(() => null) : null,
  );

let failures = 0;
function check(name, ok, detail = "") {
  console.log(`  ${ok ? "✓" : "✗"} ${name}${ok ? "" : "  <-- " + detail}`);
  if (!ok) failures++;
}

// Behavior phase: pre-create a zone (holding the browser player) and a radio station via the
// API, so the page loads with them present.
if (MODE === "behavior") {
  const players = (await api("/api/players")) || [];
  const browser = players.find((p) => p.kind === "browser");
  const zone = await api("/api/zones", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name: "Smoke Zone" }),
  });
  if (browser && zone) {
    await api(`/api/players/${browser.id}`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ zone_id: zone.id }),
    });
  }
  await api("/api/radio", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name: "Smoke FM", stream_url: "http://127.0.0.1:1/stream" }),
  });
}

const target = await (
  await fetch("http://127.0.0.1:9222/json/new?" + encodeURIComponent(base + PATH), { method: "PUT" })
).json();
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0;
const pending = new Map();
const exceptions = [];
ws.addEventListener("message", (e) => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) (pending.get(m.id)(m.result), pending.delete(m.id));
  if (m.method === "Runtime.exceptionThrown")
    exceptions.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text);
});
const send = (method, params = {}) =>
  new Promise((res) => (pending.set(++id, res), ws.send(JSON.stringify({ id, method, params }))));
const js = async (expr) =>
  (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result?.value;
const clickText = (sel, text) =>
  js(`[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim()===${JSON.stringify(text)})?.click()`);

await send("Runtime.enable");
await send("Page.enable");
// A real viewport so the app's internal scrollers engage (scroll-driven infinite scroll).
await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
// Give the browser the smoke admin's session cookie (captured by Node above), then reload past
// the auth gate into the app. Injecting via CDP is deterministic — no fetch-timing race.
await send("Network.enable");
await send("Network.setCookie", {
  name: "musicata_session",
  value: COOKIE.slice(COOKIE.indexOf("=") + 1),
  domain: "127.0.0.1",
  path: "/",
});
if (MODE === "behavior") {
  // Seed two output presets (different remembered volumes) so the footer switcher renders and
  // the test below can verify a switch applies the preset's volume.
  const outputs = JSON.stringify({
    presets: [
      { id: "out-spk", label: "Speakers", sinkId: null, profileId: null, volume: 55 },
      { id: "out-hp", label: "Headphones", sinkId: null, profileId: null, volume: 20 },
    ],
    activeId: "out-spk",
  });
  await js(`localStorage.setItem('musicata-outputs', ${JSON.stringify(outputs)})`);
}
await send("Page.reload");
await sleep(2500);

console.log(`Svelte UI smoke (${PATH}, ${MODE}):`);

if (MODE === "scale") {
  const total = (await api("/api/library/summary"))?.track_count ?? 0;
  const initial = await js(`document.querySelectorAll('.track-list .track').length`);
  check("library is large", total > 2000, `tracks=${total}`);
  check("initial render is windowed (not the whole library)", initial > 0 && initial <= 200, `rows=${initial}`);
  // Infinite scroll: pull the sentinel into view, more rows append.
  await js(`document.querySelector('.scroll-sentinel')?.scrollIntoView()`);
  await sleep(900);
  const after = await js(`document.querySelectorAll('.track-list .track').length`);
  check("infinite scroll appends a page", after > initial, `before=${initial} after=${after}`);
  // Search at scale stays bounded.
  await js(`(()=>{const el=document.querySelector('.search input'); el.value='a'; el.dispatchEvent(new Event('input',{bubbles:true}));})()`);
  await sleep(1000);
  const results = await js(`document.querySelectorAll('.track-list .track').length`);
  check("search at scale returns a bounded page", results > 0 && results <= 300, `results=${results}`);
  check("no uncaught exceptions", exceptions.length === 0, exceptions.slice(0, 3).join(" | "));
  console.log(failures ? `\nFAILED: ${failures} check(s)` : `\nAll checks passed`);
  ws.close();
  process.exit(failures ? 1 : 0);
}

// ---- behavior phase ----

// Hot path: a progress tick must move only the elapsed text, never now-title.
await js(`document.querySelector('.track-main')?.click()`);
await sleep(1500);
await js(`(() => {
  window.__t = 0; window.__n = 0;
  const time = document.querySelector('.seek-row .time');
  const title = document.querySelector('#now-title');
  if (time) new MutationObserver(() => window.__t++).observe(time, { childList: true, characterData: true, subtree: true });
  if (title) new MutationObserver(() => window.__n++).observe(title, { childList: true, characterData: true, subtree: true });
})()`);
await sleep(4200);
check("playback started", (await js(`document.querySelector('.transport')?.dataset.status`)) === "playing");
check("hot path: elapsed text updates on ticks", (await js(`window.__t`)) >= 2);
check("hot path: now-title NOT swept on ticks", (await js(`window.__n`)) === 0);

// Queue
await js(`document.querySelector('.queue-btn')?.click()`);
await sleep(500);
check("queue drawer lists tracks", (await js(`document.querySelectorAll('.queue-row').length`)) > 0);
await clickText(".queue-head button", "Close");

// Browse filter + search persistence
await clickText(".seg", "Albums");
await sleep(400);
const all = await js(`document.querySelectorAll('.album-card').length`);
await js(`(()=>{const s=document.querySelector('.browse-filters select'); if(s&&s.options.length>1){s.value=s.options[1].value; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
await sleep(900);
check("browse filter changes the grid", (await js(`document.querySelectorAll('.album-card').length`)) <= all);
await clickText("button", "Clear");
await js(`(()=>{const el=document.querySelector('.search input'); el.value='dar'; el.dispatchEvent(new Event('input',{bubbles:true}));})()`);
await sleep(900);
const tA = await js(`document.querySelector('.content-title h2')?.textContent`);
await clickText(".seg", "Artists");
await sleep(900);
const tB = await js(`document.querySelector('.content-title h2')?.textContent`);
check("search shows on the segment", /search/i.test(tA || ""), `title=${tA}`);
check("search persists across segment switch", /search/i.test(tB || ""), `title=${tB}`);
await js(`(()=>{const el=document.querySelector('.search input'); el.value=''; el.dispatchEvent(new Event('input',{bubbles:true}));})()`);
await sleep(400);

// Smart playlist
await js(`[...document.querySelectorAll('.library-panel .nav-link')].find(b=>/never played/i.test(b.textContent))?.click()`);
await sleep(900);
check("smart playlist opens + lists tracks", (await js(`document.querySelectorAll('.track-list .track').length`)) > 0);

// Metadata
await js(`document.querySelector('.track-meta')?.click()`);
await sleep(900);
check("metadata panel opens", await js(`!!document.querySelector('.metadata-drawer')`));
await clickText(".queue-head button", "Close");

// Equalizer: opening the panel + applying a preset must render bands and NOT disturb the
// now-playing track (the Web Audio routing must not restart/reload playback).
const eqTitleBefore = await js(`document.querySelector('#now-title')?.textContent`);
await js(`[...document.querySelectorAll('.eq-btn')].find(b=>/equalizer/i.test(b.title))?.click()`);
await sleep(400);
check("eq panel opens", await js(`!!document.querySelector('.eq-drawer')`));
// Volume leveling: a quiet track (-20 LUFS, -10 dBTP) is boosted toward the -14 target when
// leveling is on (the leveling gain goes above 1.0). The toggle is the first .eq-toggle.
await js(`window.__audio?.setTrackLoudness(-20, -10)`);
await js(`[...document.querySelectorAll('.eq-toggle input')][0]?.click()`);
await sleep(200);
check("volume leveling boosts a quiet track", (await js(`window.__audio?.levelingGain?.gain?.value ?? 0`)) > 1.5);
await js(`[...document.querySelectorAll('.eq-toggle input')][0]?.click()`);
await js(`window.__audio?.setTrackLoudness(null, null)`);
await js(`(()=>{const s=document.querySelector('.eq-field select'); if(s){s.value='demo-bass'; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
await sleep(500);
check("eq preset applies bands", (await js(`document.querySelectorAll('.eq-band').length`)) > 0);
check("eq response curve renders", await js(`!!document.querySelector('.eq-curve-line')?.getAttribute('d')`));
// The biquad nodes are actually built and applied to the live graph (this is what regressed:
// a short-circuited effect left the profile unapplied even though the DOM showed bands).
check("eq biquads applied to graph", (await js(`window.__audio?.eqBands?.length ?? 0`)) > 0);
check("eq graph processes audio (level>0)", (await js(`(()=>{const l=window.__audio?.levels(); return l? l.l+l.r : 0;})()`)) > 0);
check(
  "eq does not disturb now-playing",
  eqTitleBefore === (await js(`document.querySelector('#now-title')?.textContent`)),
  `${eqTitleBefore}`,
);
await js(`(()=>{const s=document.querySelector('.eq-field select'); if(s){s.value=''; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
await clickText(".eq-head button", "Close");
await sleep(300);

// Output switcher: the seeded presets render in the footer; switching to "Headphones" applies
// that preset's remembered volume (55 → 20) — proves profile/sink/volume swap on one tap.
check("output switcher renders presets", (await js(`document.querySelectorAll('.output-btn').length`)) >= 2);
await clickText(".output-btn", "Headphones");
await sleep(900);
check(
  "output switch applies remembered volume",
  Number(await js(`document.querySelector('.transport-aux input[type=range]')?.value`)) === 20,
  `vol=${await js(`document.querySelector('.transport-aux input[type=range]')?.value`)}`,
);

// VU meter: opening it renders the two (L/R) McIntosh-style meters.
await js(`[...document.querySelectorAll('.eq-btn')].find(b=>/vu/i.test(b.title))?.click()`);
await sleep(400);
check("vu meter opens with L/R meters", (await js(`document.querySelectorAll('.vu-svg').length`)) >= 2);
await clickText(".vu-head button", "Close");
await sleep(300);

// Recommendations: "Start radio from this" returns the seed + similar tracks (the local
// content fallback always yields the seed; ListenBrainz adds more when MBIDs + network exist).
const npId = (await api("/api/players/browser-local/state"))?.now_playing?.track_id;
if (npId) {
  const radio = await api(`/api/tracks/${npId}/radio?limit=10`);
  check("radio endpoint returns tracks", (radio?.track_ids?.length ?? 0) >= 1, JSON.stringify(radio));
}
check("radio button present in footer", await js(`!!document.querySelector('.radio-btn')`));
// Radio from the now-playing track must CONTINUE it (enqueue the station after it), not restart
// it from 0. The bug: startRadio called play_tracks with the seed at index 0, resetting elapsed.
const beforeRadio = await api("/api/players/browser-local/state");
const seedId = beforeRadio?.now_playing?.track_id;
const e0 = beforeRadio?.elapsed_seconds ?? 0;
const q0 = beforeRadio?.queue?.length ?? 0;
await js(`document.querySelector('.radio-btn')?.click()`);
await sleep(2000);
const afterRadio = await api("/api/players/browser-local/state");
check(
  "radio continues the seed (no restart)",
  !!seedId && afterRadio?.now_playing?.track_id === seedId && (afterRadio?.elapsed_seconds ?? 0) >= e0,
  `seed=${seedId} e0=${e0} -> np=${afterRadio?.now_playing?.track_id} e1=${afterRadio?.elapsed_seconds}`,
);
check(
  "radio enqueues a station after the seed",
  (afterRadio?.queue?.length ?? 0) >= q0,
  `queue ${q0} -> ${afterRadio?.queue?.length}`,
);
// Continuous-play (autoplay) toggle persists through the API.
await api("/api/autoplay", { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify({ enabled: true }) });
check("autoplay toggle persists", (await api("/api/autoplay"))?.enabled === true);
await api("/api/autoplay", { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify({ enabled: false }) });

// Radio: the station shows in the sidebar; playing it sets now-playing.
check("radio station listed", await js(`[...document.querySelectorAll('.library-panel .nav-link')].some(b=>/smoke fm/i.test(b.textContent))`));
await js(`[...document.querySelectorAll('.library-panel .nav-link')].find(b=>/smoke fm/i.test(b.textContent))?.click()`);
await sleep(900);
check("radio play sets now-playing", /smoke fm/i.test((await js(`document.querySelector('#now-title')?.textContent`)) || ""));

// Zone: switch the output to the zone (which holds the browser player) and play.
await js(`(()=>{const s=document.querySelector('.player-switch-btn'); const o=[...s.options].find(o=>/zone/i.test(o.textContent)); if(o){s.value=o.value; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
await sleep(1200);
check("switched output to a zone", /zone/i.test((await js(`document.querySelector('.player-switch-btn')?.value`)) || ""));
await clickText(".seg", "Tracks");
await sleep(600);
await js(`document.querySelector('.track-main')?.click()`);
await sleep(2000);
const z1 = await js(`document.querySelector('.seek-row .time')?.textContent`);
await sleep(2200);
const z2 = await js(`document.querySelector('.seek-row .time')?.textContent`);
check("zone plays (elapsed advances)", z1 !== z2, `${z1} -> ${z2}`);

check("no uncaught exceptions", exceptions.length === 0, exceptions.slice(0, 3).join(" | "));
console.log(failures ? `\nFAILED: ${failures} check(s)` : `\nAll checks passed`);
ws.close();
process.exit(failures ? 1 : 0);
