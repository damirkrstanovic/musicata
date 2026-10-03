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
  const saved = await api('/api/playlists', {
    method: 'POST', headers: {'content-type': 'application/json'},
    body: JSON.stringify({name: 'Phone playlist'}),
  });
  const tracks = await api('/api/tracks?limit=2');
  await api(`/api/playlists/${saved.id}`, {
    method: 'PATCH', headers: {'content-type': 'application/json'},
    body: JSON.stringify({add_track_ids: tracks.items.map(t => t.id)}),
  });
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
  await api("/api/zones", {
    method: "POST", headers: {"content-type": "application/json"},
    body: JSON.stringify({name: "Z Remote output"}),
  });
  await api("/api/radio", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name: "Smoke FM", stream_url: "http://127.0.0.1:1/stream" }),
  });
  // Continuous play defaults on; turn it off so the autoplay loop doesn't grow the queue
  // under the queue/playback assertions below.
  await api("/api/autoplay", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ enabled: false }),
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
// The app is served under a strict Content-Security-Policy (`default-src 'self'`), which is
// enforceable only because every internet fetch was moved behind the API. A violation is NOT a
// Runtime exception — Chrome reports it in the Log domain — so watch for it explicitly.
// Without this, a blocked image or media load degrades silently and the suite still passes.
const cspViolations = [];
ws.addEventListener("message", (e) => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) (pending.get(m.id)(m.result), pending.delete(m.id));
  if (m.method === "Log.entryAdded" && m.params.entry?.source === "security") {
    cspViolations.push(m.params.entry.text);
  }
  if (m.method === "Runtime.exceptionThrown") {
    const d = m.params.exceptionDetails;
    // Ignore errors thrown by browser extensions injected into the page (chrome-extension://…):
    // not app bugs, and a page reload re-triggers a flaky one in this headless profile.
    const origin = d.url || d.stackTrace?.callFrames?.[0]?.url || "";
    if (origin.startsWith("chrome-extension://")) return;
    exceptions.push(d.exception?.description || d.text);
  }
});
const send = (method, params = {}) =>
  new Promise((res) => (pending.set(++id, res), ws.send(JSON.stringify({ id, method, params }))));
const js = async (expr) =>
  (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result?.value;
const clickText = (sel, text) =>
  js(`[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim()===${JSON.stringify(text)})?.click()`);
// Poll `boolExpr` (evaluated in the page) until true, returning the elapsed ms, or Infinity if it
// never became true within `budgetMs`. Used for the latency checks below. Coarse by design — the
// value includes CDP round-trips + the poll granularity — so budgets are generous.
async function waitUntil(boolExpr, budgetMs, pollMs = 50) {
  const start = Date.now();
  do {
    if (await js(`!!(${boolExpr})`)) return Date.now() - start;
    await sleep(pollMs);
  } while (Date.now() - start < budgetMs);
  return Infinity;
}

await send("Runtime.enable");
await send("Log.enable"); // CSP violations arrive as Log.entryAdded, not Runtime exceptions.
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

// Desktop shares the listening destinations, while keeping the library visible.
if (MODE === "behavior") {
  await js(`localStorage.removeItem('musicata.browse-view'); history.replaceState(null, '')`);
  await send('Page.reload');
  await sleep(1200);
  check('desktop starts with album browsing', await js(`document.querySelectorAll('.album-card').length > 0`));
  await mobileScreenshot('desktop-album-home');
  check('desktop install prompt leaves browsing tabs clickable', await js(`(() => {
    const b = [...document.querySelectorAll('.seg')].find(b => b.textContent.trim() === 'Artists');
    const r = b.getBoundingClientRect();
    return b.contains(document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2));
  })()`));
  await clickText('.seg', 'Artists');
  await js(`history.replaceState(null, '')`);
  await send('Page.reload');
  await sleep(1200);
  check('desktop remembers browsing choice on a new visit', await js(`document.querySelectorAll('.artist-card').length > 0`));
  await clickText('.library-nav button', 'Playlists');
  await sleep(400);
  check('desktop exposes saved playlists destination', await js(`document.querySelectorAll('.saved-playlist').length > 0`));
  await clickText('.library-nav button', 'Library');
  await sleep(400);
  check('Library returns to preferred browsing view', await js(`document.querySelectorAll('.artist-card').length > 0`));
  await js(`document.querySelector('.now-title-button')?.click()`);
  await sleep(300);
  check('desktop title opens Now Playing beside the library', await js(`document.querySelector('.shell').classList.contains('np-open') && !!document.querySelector('.right-rail .queue-drawer') && document.querySelector('.content').getBoundingClientRect().right <= document.querySelector('.right-rail').getBoundingClientRect().left + 1`));
  check('desktop Now Playing does not offer a duplicate queue overlay', await js(`!document.querySelector('.queue-btn')?.getClientRects().length`));
  await mobileScreenshot('desktop-now-playing');
  await js('history.back()');
  await sleep(300);
  check('desktop Back closes Now Playing and preserves browsing', await js(`!document.querySelector('.shell').classList.contains('np-open') && document.querySelectorAll('.artist-card').length > 0`));
  await js('history.forward()');
  await sleep(300);
  check('desktop Forward restores Now Playing', await js(`document.querySelector('.shell').classList.contains('np-open') && !!document.querySelector('.right-rail .queue-drawer')`));
  await js(`localStorage.setItem('musicata.browse-view', 'removed-view'); history.replaceState(null, '')`);
  await send('Page.reload');
  await sleep(1200);
  check('invalid browsing preference falls back to Albums', await js(`document.querySelectorAll('.album-card').length > 0`));
}
// The remaining playback and scale checks explicitly exercise the track table.
await clickText('.seg', 'Tracks');
await sleep(600);

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
  // "Show the artwork" (against the real library): a cover <img> only renders for albums that
  // have art, and naturalWidth>0 means the bytes actually loaded over the authenticated artwork
  // endpoint — not just that the element is in the DOM. This is the UI-layer guard the
  // artist-artwork regression lacked. (testdata has no album covers, so this must run at scale.)
  await js(`(()=>{const el=document.querySelector('.search input'); if(el){el.value=''; el.dispatchEvent(new Event('input',{bubbles:true}));}})()`);
  await sleep(400);
  await clickText(".seg", "Albums");
  await sleep(800);
  let coverLoaded = 0;
  for (let i = 0; i < 15 && !coverLoaded; i++) {
    coverLoaded = await js(
      `[...document.querySelectorAll('.album-card .card-cover img')].filter(im=>im.complete && im.naturalWidth>0).length`,
    );
    if (!coverLoaded) {
      await js(`document.querySelector('.album-card:last-child')?.scrollIntoView()`);
      await sleep(400);
    }
  }
  check("album artwork renders (img bytes loaded)", coverLoaded > 0, `loaded=${coverLoaded}`);
  // Returning deep into a paged grid must rebuild enough pages to restore scroll.
  await js(`document.querySelector('.scroll-sentinel')?.scrollIntoView()`);
  await sleep(800);
  await js(`document.querySelector('.album-card:last-child')?.scrollIntoView()`);
  await sleep(500);
  const deepScroll = await js(`document.querySelector('.content').scrollTop`);
  const deepCount = await js(`document.querySelectorAll('.album-card').length`);
  await js(`document.querySelector('.album-card:last-child .card-text')?.click()`);
  await sleep(500);
  await js(`document.querySelector('.back-btn')?.click()`);
  check("Back restores a paged album grid", await waitUntil(`document.querySelectorAll('.album-card').length > 60 && Math.abs(document.querySelector('.content').scrollTop - ${deepScroll}) < 4`, 5000) < Infinity, `cards=${deepCount}, scroll=${deepScroll}`);

  check("no uncaught exceptions", exceptions.length === 0, exceptions.slice(0, 3).join(" | "));
  check("no CSP violations", cspViolations.length === 0, cspViolations.slice(0, 3).join(" | "));
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

// A real SQLite writer must not delay Pause or prevent the output WebSocket
// from carrying it. Hold the write lock through a periodic progress checkpoint.
if (process.env.MUSICATA_SMOKE_DB) {
  const {spawn} = await import('node:child_process');
  const locker = spawn('python3', ['-u', '-c', `
import sqlite3, sys
c = sqlite3.connect(sys.argv[1], timeout=20)
c.execute('BEGIN IMMEDIATE')
print('locked', flush=True)
sys.stdin.readline()
c.rollback()
`, process.env.MUSICATA_SMOKE_DB]);
  try {
    await new Promise((resolve, reject) => {
      locker.stdout.once('data', resolve);
      locker.once('error', reject);
      locker.once('exit', code => { if (code) reject(new Error('lock helper failed')); });
    });
    await sleep(11000);
    await js(`(() => {
      window.__pauseRequest = 'not sent';
      const original = window.fetch;
      window.fetch = async (...args) => {
        const start = performance.now();
        const response = await original(...args);
        if (String(args[0]).includes('/commands')) window.__pauseRequest = {status: response.status, ms: performance.now() - start};
        return response;
      };
    })()`);
    await js(`document.querySelector('.transport-buttons .control.play')?.click()`);
    const pauseMs = await waitUntil(`document.querySelector('.transport')?.dataset.status === 'paused' && document.querySelector('audio')?.paused`, 2000);
    check('busy database: Pause stops actual browser audio promptly', pauseMs < 1000,
      `${pauseMs}ms; ` + JSON.stringify(await js(`({status:document.querySelector('.transport')?.dataset.status, paused:document.querySelector('audio')?.paused, request:window.__pauseRequest})`)));
  } finally {
    locker.stdin.end('release\n');
    await new Promise(resolve => locker.once('exit', resolve));
  }
  await js(`document.querySelector('.transport-buttons .control.play')?.click()`);
  await waitUntil(`document.querySelector('.transport')?.dataset.status === 'playing'`, 3000);
}

// ---- Transport: the core music-playing controls (pause/resume, skip, seek) ----
// Clicking a track queued the whole list (playTracks), so next/previous have somewhere to go.
const titleA = await js(`document.querySelector('#now-title')?.textContent`);
// Pause → paused; play → playing again, on the SAME track (resume must not reload).
await js(`document.querySelector('.transport-buttons .control.play')?.click()`);
await sleep(700);
check("pause halts playback", (await js(`document.querySelector('.transport')?.dataset.status`)) === "paused");
await js(`document.querySelector('.transport-buttons .control.play')?.click()`);
await sleep(700);
check("resume returns to playing", (await js(`document.querySelector('.transport')?.dataset.status`)) === "playing");
check("resume keeps the same track", (await js(`document.querySelector('#now-title')?.textContent`)) === titleA, `${titleA}`);
// Next → a different track becomes current.
await js(`[...document.querySelectorAll('.transport-buttons .control')].find(b=>b.title==='Next')?.click()`);
await sleep(1800);
const titleB = await js(`document.querySelector('#now-title')?.textContent`);
check("next advances to another track", !!titleB && titleB !== titleA, `${titleA} -> ${titleB}`);
// Previous → back to the original track (the browser player steps the queue index, no restart).
await js(`[...document.querySelectorAll('.transport-buttons .control')].find(b=>b.title==='Previous')?.click()`);
await sleep(1800);
check("previous steps back a track", (await js(`document.querySelector('#now-title')?.textContent`)) === titleA, `back to ${titleA}`);
// Seek → the elapsed clock jumps forward to the dragged position (tracks are ≥86s here).
const seekBefore = await js(`(()=>{const i=document.querySelector('input.seek'); return i?Number(i.value):0;})()`);
await js(`(()=>{const i=document.querySelector('input.seek'); if(!i) return; const dur=Number(i.max)||0; const tgt=Math.max(2,Math.floor(dur*0.5)); i.value=tgt; i.dispatchEvent(new Event('input',{bubbles:true})); i.dispatchEvent(new Event('change',{bubbles:true}));})()`);
await sleep(1200);
const seekAfter = await js(`(()=>{const i=document.querySelector('input.seek'); return i?Number(i.value):0;})()`);
check("seek jumps the playback position", seekAfter > seekBefore + 3, `before=${seekBefore} after=${seekAfter}`);

// Favorite a track: the heart toggles (aria-pressed flips) and persists via the favorites store.
const favSel = `.track-list .track button[aria-pressed]`;
const favBefore = await js(`document.querySelector('${favSel}')?.getAttribute('aria-pressed')`);
await js(`document.querySelector('${favSel}')?.click()`);
await sleep(500);
const favAfter = await js(`document.querySelector('${favSel}')?.getAttribute('aria-pressed')`);
check("favorite toggles a track", !!favBefore && favAfter !== favBefore, `${favBefore} -> ${favAfter}`);
await js(`document.querySelector('${favSel}')?.click()`); // restore
await sleep(300);

// Queue (list + editing)
await js(`document.querySelector('.queue-btn')?.click()`);
await sleep(500);
check("queue drawer lists tracks", (await js(`document.querySelectorAll('.queue-row').length`)) > 0);
// Reorder: move the first row down → a different track heads the queue.
const qFirst = await js(`document.querySelector('.queue-row .q-title')?.textContent`);
const qLen = await js(`document.querySelectorAll('.queue-row').length`);
await js(`document.querySelector('.queue-row .q-actions button[title="Move down"]')?.click()`);
await sleep(500);
check("queue reorder moves an item", (await js(`document.querySelector('.queue-row .q-title')?.textContent`)) !== qFirst, `was ${qFirst}`);
// Remove the last row → the queue shrinks by one.
await js(`(()=>{const r=[...document.querySelectorAll('.queue-row')]; r.at(-1)?.querySelector('button[title="Remove"]')?.click();})()`);
await sleep(500);
check("queue remove drops a row", (await js(`document.querySelectorAll('.queue-row').length`)) < qLen, `len ${qLen}`);
await clickText(".queue-head button", "Close");

// Browse filter + search persistence
await clickText(".seg", "Albums");
await sleep(400);
const all = await js(`document.querySelectorAll('.album-card').length`);
await js(`(()=>{const s=document.querySelector('.browse-filters select'); if(s&&s.options.length>1){s.value=s.options[1].value; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
await sleep(900);
check("browse filter changes the grid", (await js(`document.querySelectorAll('.album-card').length`)) <= all);
await clickText("button", "Clear");
await sleep(400);
// Play an album from the cover ▶ (a primary way to start music): pause first, then the album
// play button must resume playback (proving the control plays the album, not just that audio ran).
await js(`document.querySelector('.transport-buttons .control.play')?.click()`); // pause (currently playing)
await sleep(400);
await js(`document.querySelector('.album-card .card-play')?.click()`);
await sleep(2000);
check("play album from the grid starts playback", (await js(`document.querySelector('.transport')?.dataset.status`)) === "playing");
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

// Metadata: open the review panel and approve a field (✓). setField PATCHes then re-fetches,
// so the button reactively shows `active` only if the approval actually persisted server-side.
await js(`document.querySelector('.track-meta')?.click()`);
await sleep(900);
check("metadata panel opens", await js(`!!document.querySelector('.metadata-drawer')`));
await js(`document.querySelector('.meta-field .icon-button')?.click()`); // ✓ approve the first field
await sleep(700);
check("metadata field approval persists", await js(`!!document.querySelector('.meta-field .icon-button.active')`));
await clickText(".queue-head button", "Close");

// Equalizer: opening the panel + applying a preset must render bands and NOT disturb the
// now-playing track (the Web Audio routing must not restart/reload playback).
const eqTitleBefore = await js(`document.querySelector('#now-title')?.textContent`);
await js(`[...document.querySelectorAll('.eq-btn')].find(b=>/equalizer/i.test(b.title))?.click()`);
await sleep(400);
check("eq panel opens", await js(`!!document.querySelector('.eq-drawer')`));
// Volume leveling. Track quiet (-20 LUFS) but album louder (-8 LUFS): track mode boosts the
// quiet track (gain > 1), album mode uses the album aggregate instead (here → attenuation),
// proving the mode selector picks the right LUFS.
const setLeveling = (m) =>
  js(`(()=>{const s=document.querySelector('.leveling-select'); if(s){s.value=${JSON.stringify(m)}; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
await js(`window.__audio?.setTrackLoudness(-20, -10, -8, -2)`);
await setLeveling("track");
await sleep(200);
check("volume leveling boosts a quiet track", (await js(`window.__audio?.levelingGain?.gain?.value ?? 0`)) > 1.5);
await setLeveling("album");
await sleep(200);
check("album leveling uses the album loudness", (await js(`window.__audio?.levelingGain?.gain?.value ?? 1`)) < 1);
await setLeveling("off");
await js(`window.__audio?.setTrackLoudness(null, null)`);
// The Preset picker is a separate .eq-field select (not the leveling one).
await js(`(()=>{const s=document.querySelector('.eq-field select:not(.leveling-select)'); if(s){s.value='demo-bass'; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
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
// Listening stats view: open it from the footer and confirm it renders figures from
// /api/history/stats (12 stat rows, even when history is empty → zeros).
await js(`[...document.querySelectorAll('button')].find(b=>/listening stats/i.test(b.title))?.click()`);
await sleep(400);
check("stats panel opens", await js(`!!document.querySelector('section[aria-label="Listening stats"]')`));
check(
  "stats panel shows figures",
  (await js(`document.querySelectorAll('section[aria-label="Listening stats"] .stat-row').length`)) >= 12,
);
await js(`document.querySelector('section[aria-label="Listening stats"] .ghost-button')?.click()`);
// AutoEq picker: search the bundled curated set + pick a real model → it becomes a saved profile.
await js(`(()=>{
  const d=[...document.querySelectorAll('details.eq-import')].find(x=>x.querySelector('summary')?.textContent.includes('Pick your headphone'));
  if(d){ d.open=true; const i=d.querySelector('input'); if(i){ i.value='HD 600'; i.dispatchEvent(new Event('input',{bubbles:true})); } }
})()`);
// The ~850 KB index and the chosen preset are both fetched through the server now (the CSP
// forbids the page reaching AutoEq directly), so wait on the condition rather than assuming a
// fixed latency — the relay adds a hop and a cold TLS handshake on the first call.
const hpListed = await waitUntil(`document.querySelectorAll('.hp-matches button').length > 0`, 15000);
check("autoeq index loads through the API", hpListed !== Infinity, await js(`document.querySelector('.eq-import .eq-note.error')?.textContent || 'no matches rendered'`));
await js(`[...document.querySelectorAll('.hp-matches button')].find(b=>b.textContent.includes('HD 600'))?.click()`);
check(
  "autoeq picker saves a headphone profile",
  await waitUntil(
    `[...document.querySelectorAll('.eq-field select option')].some(o=>o.textContent.includes('HD 600'))`,
    15000,
  ) !== Infinity,
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

// Room correction: upload a WAV impulse response to a profile and confirm it's recorded + served.
{
  const wav = new ArrayBuffer(48);
  const dv = new DataView(wav);
  const w = (o, s) => { for (let i = 0; i < s.length; i++) dv.setUint8(o + i, s.charCodeAt(i)); };
  w(0, "RIFF"); dv.setUint32(4, 40, true); w(8, "WAVE");
  w(12, "fmt "); dv.setUint32(16, 16, true); dv.setUint16(20, 1, true); dv.setUint16(22, 2, true);
  dv.setUint32(24, 48000, true); dv.setUint32(28, 192000, true); dv.setUint16(32, 4, true); dv.setUint16(34, 16, true);
  w(36, "data"); dv.setUint32(40, 4, true);
  await fetch(base + "/api/dsp/profiles/smoke-room", {
    method: "PUT",
    headers: { "content-type": "application/json", cookie: COOKIE },
    body: JSON.stringify({ id: "smoke-room", name: "Smoke Room", preampDb: 0, bands: [], kind: "speakers" }),
  });
  const up = await fetch(base + "/api/dsp/profiles/smoke-room/impulse", {
    method: "POST",
    headers: { cookie: COOKIE },
    body: wav,
  });
  const list = (await api("/api/dsp/profiles")) || [];
  const saved = list.find((p) => p.id === "smoke-room");
  const served = await fetch(base + "/api/dsp/profiles/smoke-room/impulse", { headers: { cookie: COOKIE } });
  check(
    "room IR uploads, records sampleRate, and serves back",
    up.ok && saved?.roomIr?.sampleRate === 48000 && served.ok,
    `up=${up.status} sr=${saved?.roomIr?.sampleRate} get=${served.status}`,
  );

  // Server-side Snapcast EQ: selecting a DSP profile for the multi-room output persists +
  // reflects in status (the in-process correction is applied to the writer when running).
  const patched = await fetch(base + "/api/snapcast/status", {
    method: "PATCH",
    headers: { "content-type": "application/json", cookie: COOKIE },
    body: JSON.stringify({ dsp_profile_id: "smoke-room" }),
  });
  const snap = patched.ok ? await patched.json() : null;
  check("snapcast server-side DSP profile selection persists", snap?.dsp_profile_id === "smoke-room");
}

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
  check("radio endpoint returns tracks", (radio?.tracks?.length ?? 0) >= 1, JSON.stringify(radio));
}
check("radio button present in footer", await js(`!!document.querySelector('.radio-btn')`));
// Radio from the now-playing track must CONTINUE it (enqueue the station after it), not restart
// it from 0. The bug: startRadio called play_tracks with the seed at index 0, resetting elapsed.
const beforeRadio = await api("/api/players/browser-local/state");
const seedId = beforeRadio?.now_playing?.track_id;
const e0 = beforeRadio?.elapsed_seconds ?? 0;
const q0 = beforeRadio?.queue?.length ?? 0;
// Recommendation lookup may take a while. The click must immediately move to Mix and describe
// that work instead of looking like a missed tap.
await js(`(() => {
  const originalFetch = window.fetch.bind(window);
  const pending = [];
  window.__radioPendingCount = 0;
  window.__radioReleasedCount = 0;
  window.__releaseHeldRadio = () => pending.shift()?.();
  window.__restoreRadioFetch = () => { window.fetch = originalFetch; };
  window.fetch = (input, init) => {
    const url = input instanceof Request ? input.url : String(input);
    if (url.includes('/audio-radio?')) {
      return Promise.resolve(new Response(JSON.stringify({ tracks: [] }), {
        headers: { 'content-type': 'application/json' },
      }));
    }
    if (url.includes('/radio?')) {
      if (window.__failNextRadio) {
        window.__failNextRadio = false;
        return Promise.reject(new Error('Recommendation service unavailable'));
      }
      window.__radioPendingCount++;
      return new Promise((resolve, reject) => pending.push(() => originalFetch(input, init).then(
        value => { window.__radioReleasedCount++; resolve(value); },
        error => { window.__radioReleasedCount++; reject(error); },
      )));
    }
    return originalFetch(input, init);
  };
})()`);
await js(`document.querySelector('.radio-btn')?.click()`);
check(
  "radio click immediately shows loading feedback",
  await js(`document.querySelector('[data-mix-status="loading"]')?.textContent?.includes("Finding more tracks")`),
);
await js(`window.__releaseHeldRadio?.()`);
check(
  "released radio request populates the live mix",
  await waitUntil(`document.querySelectorAll('.mix-queue .queue-row').length > 0`, 3000) < Infinity,
);
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
const queuedBeforeAppend = afterRadio?.queue?.length ?? 0;
const appendTrack = (await api("/api/tracks?limit=2"))?.items?.find((track) => track.id !== seedId);
if (appendTrack) {
  await api("/api/players/browser-local/commands", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ command: "enqueue", track_ids: [appendTrack.id] }),
  });
  check(
    "appended track reaches the live mix",
    await waitUntil(`document.querySelectorAll('.mix-queue .queue-row').length > ${queuedBeforeAppend}`, 3000) < Infinity,
  );
  const liveCount = await js(`document.querySelectorAll('.mix-queue .queue-row').length`);
  check("mix follows tracks appended after generation", liveCount > queuedBeforeAppend, `${queuedBeforeAppend} -> ${liveCount}`);
  const beforeSelection = await api("/api/players/browser-local/state");
  const selectedIndex = liveCount - 1;
  await js(`document.querySelector('.mix-queue .queue-row[data-index="${selectedIndex}"] .q-main')?.click()`);
  await sleep(400);
  const afterSelection = await api("/api/players/browser-local/state");
  const originalIds = beforeSelection.queue.map((item) => item.track_id);
  // Selecting the tail can legitimately trigger autoplay to append more tracks.
  // Preserve the original queue prefix and select the requested item, not a fixed length.
  check(
    "mix row plays by queue index without replacing the live queue",
    afterSelection?.queue_position === selectedIndex &&
      JSON.stringify(afterSelection?.queue?.slice(0, originalIds.length).map((item) => item.track_id)) === JSON.stringify(originalIds),
    `position ${afterSelection?.queue_position}, queue ${originalIds.length} -> ${afterSelection?.queue?.length}`,
  );
}

// A held older response must not replace a newer radio request, or spill onto a newly selected
// output. Audio-radio has a deterministic empty response; normal radio remains held.
await js(`document.querySelector('.radio-btn')?.click()`);
check("older radio request is held", await waitUntil(`window.__radioPendingCount === 2`, 1000) < Infinity);
await clickText(".seg", "Tracks");
check("track browser opens for newer request", await waitUntil(`document.querySelector('.track-audio-radio')`, 1000) < Infinity);
await js(`document.querySelector('.track-audio-radio')?.click()`);
check(
  "newer audio-radio request completes",
  await waitUntil(`document.querySelector('[data-mix-status="empty"]')`, 1000) < Infinity,
);
const newerMix = await js(`document.querySelector('[data-mix-status="empty"]')?.textContent`);
await js(`window.__releaseHeldRadio?.()`);
check("older held request finishes", await waitUntil(`window.__radioReleasedCount === 2`, 3000) < Infinity);
check(
  "older radio response cannot overwrite a newer mix",
  (await js(`document.querySelector('[data-mix-status="empty"]')?.textContent`)) === newerMix,
);
await js(`window.__failNextRadio = true; document.querySelector('.radio-btn')?.click()`);
await waitUntil(`document.querySelector('[data-mix-status="error"]')`, 1000);
check(
  "radio request errors are visible in Mix",
  /recommendation service unavailable/i.test((await js(`document.querySelector('[data-mix-status="error"]')?.textContent`)) || ""),
);
await js(`document.querySelector('.radio-btn')?.click()`);
check("output-switch radio request is held", await waitUntil(`window.__radioPendingCount === 3`, 1000) < Infinity);
const oldOutputQueueBeforeSwitch = (await api("/api/players/browser-local/state"))?.queue?.length ?? 0;
await js(`(()=>{const s=document.querySelector('.player-switch-btn'); const o=[...s.options].find(o=>/smoke zone/i.test(o.textContent)); if(o){s.value=o.value; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
check(
  "output switch completes before stale response release",
  await waitUntil(`/smoke zone/i.test(document.querySelector('.player-switch-btn option:checked')?.textContent || '')`, 3000) < Infinity,
);
await js(`window.__releaseHeldRadio?.()`);
check("switched-output held request finishes", await waitUntil(`window.__radioReleasedCount === 3`, 3000) < Infinity);
check(
  "stale radio response stays off a newly selected output",
  /different output/i.test((await js(`document.querySelector('.content')?.textContent`)) || ""),
);
check(
  "stale radio response does not change its original output queue",
  ((await api("/api/players/browser-local/state"))?.queue?.length ?? 0) === oldOutputQueueBeforeSwitch,
);
await js(`window.__restoreRadioFetch?.()`);
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

// Clear the queue → it empties (the Clear control, end-to-end on the active player).
await js(`document.querySelector('.queue-btn')?.click()`);
await sleep(500);
await clickText(".queue-head button", "Clear");
await sleep(800);
check("clear empties the queue", (await js(`document.querySelectorAll('.queue-row').length`)) === 0);
await clickText(".queue-head button", "Close");

// ---- Latency: client-side responsiveness of the core paths, against the local testdata fixture
// with deliberately GENEROUS budgets. These measure client + server-logic + WS-broadcast time —
// NOT SMB read latency (that needs the live share, and is machine/network dependent). The point
// is to catch a gross regression (a path that got much slower or stopped responding), not to pin
// a millisecond budget. Each playback check also asserts audio actually advances, so a fast-but-
// silent "playing" still fails.
await js(`(()=>{const s=document.querySelector('.player-switch-btn'); const o=[...s.options].find(o=>!/zone/i.test(o.textContent)); if(o){s.value=o.value; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`); // output → the browser player
await sleep(700);
await clickText(".seg", "Tracks");
await sleep(500);

// Establish a clean, really-playing baseline on the browser player: click a track, wait until the
// status is playing AND the elapsed clock has ticked (proof of actual audio). All measurements
// below are same-track transitions from here, so they aren't confounded by track-load or by a
// stale state arriving after the output switch.
await js(`document.querySelector('.track-main')?.click()`);
await waitUntil(`document.querySelector('.transport')?.dataset.status === "playing"`, 5000);
const warmTime = await js(`document.querySelector('.seek-row .time')?.textContent`);
await waitUntil(`document.querySelector('.seek-row .time')?.textContent !== ${JSON.stringify(warmTime)}`, 4000);

// Pause: Play button → status reads paused.
await js(`document.querySelector('.transport-buttons .control.play')?.click()`);
const pauseMs = await waitUntil(`document.querySelector('.transport')?.dataset.status === "paused"`, 3000);
check("latency: pause responds within budget", pauseMs < 2000, `${pauseMs}ms`);

// Resume to audio: Play button → status playing AND the elapsed clock advances again (real audio
// back, not just a status flip). Measured from the paused clock value so a false "playing" fails.
const pausedTime = await js(`document.querySelector('.seek-row .time')?.textContent`);
await js(`document.querySelector('.transport-buttons .control.play')?.click()`);
const resumeMs = await waitUntil(
  `document.querySelector('.transport')?.dataset.status === "playing" && document.querySelector('.seek-row .time')?.textContent !== ${JSON.stringify(pausedTime)}`,
  5000,
);
check("latency: resume to real audio within budget", resumeMs < 4000, `${resumeMs}ms`);

// Next: click Next → a different track becomes now-playing.
const beforeNext = await js(`document.querySelector('#now-title')?.textContent`);
await js(`[...document.querySelectorAll('.transport-buttons .control')].find(b=>b.title==='Next')?.click()`);
const nextMs = await waitUntil(
  `document.querySelector('#now-title')?.textContent !== ${JSON.stringify(beforeNext)}`,
  4000,
);
check("latency: next changes track within budget", nextMs < 3000, `${nextMs}ms`);

// Search-to-render: type a query → the search results title appears.
await js(`(()=>{const el=document.querySelector('.search input'); el.value='the'; el.dispatchEvent(new Event('input',{bubbles:true}));})()`);
const searchMs = await waitUntil(`/search/i.test(document.querySelector('.content-title h2')?.textContent || "")`, 3000);
check("latency: search renders within budget", searchMs < 2500, `${searchMs}ms`);
await js(`(()=>{const el=document.querySelector('.search input'); el.value=''; el.dispatchEvent(new Event('input',{bubbles:true}));})()`);
await sleep(400);

// Resume after restart: reopening the app (a page reload) restores the persisted now-playing
// track, and the footer Play must actually play it on THIS tab. The bug — the footer sent a bare
// `play` while the freshly-loaded tab had never claimed browser output, so `drive()` skipped it
// and nothing played, even though picking a track from the library (which claims) worked.
await js(`(()=>{const s=document.querySelector('.player-switch-btn'); const o=[...s.options].find(o=>!/zone/i.test(o.textContent)); if(o){s.value=o.value; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`); // output → the browser player itself, not the zone
await sleep(800);
await clickText(".seg", "Tracks");
await sleep(600);
await js(`document.querySelector('.track-main')?.click()`); // play a track on the browser player
await sleep(1500);
await js(`document.querySelector('.transport-buttons .control.play')?.click()`); // pause → a persisted, paused now-playing
await sleep(500);
const restartTitle = await js(`document.querySelector('#now-title')?.textContent`);
await send("Page.reload"); // reopen the app: a fresh tab that has NOT claimed output
await sleep(2800); // reconnect + restore the persisted session
check("restart restores the now-playing track", (await js(`document.querySelector('#now-title')?.textContent`)) === restartTitle, `${restartTitle}`);
const r1 = await js(`document.querySelector('.seek-row .time')?.textContent`);
await js(`document.querySelector('.transport-buttons .control.play')?.click()`); // footer Play on the never-claimed tab
await sleep(2600);
check("restart resume goes to playing", (await js(`document.querySelector('.transport')?.dataset.status`)) === "playing");
const r2 = await js(`document.querySelector('.seek-row .time')?.textContent`);
check("restart resume actually plays on this tab (elapsed advances)", !!r1 && r1 !== r2, `${r1} -> ${r2}`);

// Switching away from a playing browser must stop its stream and must never
// forward its old audio events into the newly selected remote output's socket.
check("output handoff starts with local audio playing", await js(`!document.querySelector('audio').paused`));
await js(`(() => {
  window.__outputReports = [];
  const send = WebSocket.prototype.send;
  WebSocket.prototype.send = function(data) { window.__outputReports.push(data); return send.call(this, data); };
  const select = document.querySelector('.player-switch-btn');
  select.value = [...select.options].find(o => o.textContent.includes('Z Remote output')).value;
  select.dispatchEvent(new Event('change', {bubbles: true}));
})()`);
await sleep(1300);
check("switching output pauses local audio", await js(`document.querySelector('audio').paused`));
await js(`document.querySelector('audio').dispatchEvent(new Event('ended'))`);
await sleep(200);
check("old browser progress and ended do not reach remote output", await js(`window.__outputReports.every(data => !['progress', 'ended'].includes(JSON.parse(data).type))`));
await js(`(() => { const s = document.querySelector('.player-switch-btn'); s.value = [...s.options].find(o => o.textContent === 'This Browser').value; s.dispatchEvent(new Event('change', {bubbles: true})); })()`);
await sleep(600);

check("no uncaught exceptions", exceptions.length === 0, exceptions.slice(0, 3).join(" | "));
// Mobile regression: CSS used to hide the desktop controls without rendering their
// mobile replacements. Hit-test and tap real controls, rather than DOM .click()
// (which would also activate off-screen or covered controls).
async function tapMobile(selector, scroll = false) {
  if (scroll) await js(`document.querySelector(${JSON.stringify(selector)})?.scrollIntoView({block: 'center'})`);
  const point = await js(`(() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) return null;
    const r = el.getBoundingClientRect();
    const x = r.x + r.width / 2, y = r.y + r.height / 2;
    if (r.width < 1 || r.height < 1 || x < 0 || y < 0 || x >= innerWidth || y >= innerHeight) return null;
    return el.contains(document.elementFromPoint(x, y)) ? {x, y} : null;
  })()`);
  if (!point) return false;
  await send("Input.dispatchTouchEvent", {type: "touchStart", touchPoints: [point]});
  await send("Input.dispatchTouchEvent", {type: "touchEnd", touchPoints: []});
  await sleep(350);
  return true;
}
async function mobileScreenshot(name) {
  if (!process.env.MUSICATA_UI_SCREENSHOTS) return;
  const { mkdir, writeFile } = await import('node:fs/promises');
  const dir = process.env.MUSICATA_UI_SCREENSHOTS;
  await mkdir(dir, {recursive: true});
  const {data} = await send('Page.captureScreenshot', {format: 'png'});
  await writeFile(`${dir}/${name}.png`, Buffer.from(data, 'base64'));
}
await send("Emulation.setTouchEmulationEnabled", {enabled: true});
for (const [width, height] of [[360, 800], [800, 360], [320, 568]]) {
  const label = `mobile ${width}x${height}`;
  await send("Emulation.setDeviceMetricsOverride", {width, height, deviceScaleFactor: 1, mobile: true});
  await sleep(350);
  await mobileScreenshot(`${width}x${height}-browse`);
  check(`${label}: output is labelled on the main screen`, await js(`document.querySelector('.mobile-output')?.textContent.includes('Output:')`));
  check(`${label}: output button opens selection`, await tapMobile('.mobile-output'));
  check(`${label}: output selector receives focus`, await js(`document.activeElement?.classList.contains('player-switch-btn')`));
  await tapMobile('[aria-label="Close Now Playing"]');
  const priorStatus = await js('document.querySelector(".transport")?.dataset.status');
  check(`${label}: playback control is reachable`, await tapMobile('.mini-controls .play'));
  check(`${label}: playback toggles`, await waitUntil(`document.querySelector('.transport')?.dataset.status !== ${JSON.stringify(priorStatus)}`, 2000) < Infinity);
  const priorTitle = await js('document.querySelector("#now-title")?.textContent');
  check(`${label}: next is reachable`, await tapMobile('.mini-controls [aria-label="Next"]'));
  check(`${label}: next changes track`, await waitUntil(`document.querySelector('#now-title')?.textContent !== ${JSON.stringify(priorTitle)}`, 2000) < Infinity);
  check(`${label}: queue button is reachable`, await tapMobile('.mini-controls [aria-label="Queue"]'));
  check(`${label}: queue opens`, await waitUntil('document.querySelector(".queue-drawer")', 1000) < Infinity);
  check(`${label}: queue receives focus`, await js(`document.activeElement?.getAttribute('aria-label') === 'Close queue'`));
  await mobileScreenshot(`${width}x${height}-queue`);
  const queueTitle = await js('document.querySelector(".queue-row .q-title")?.textContent');
  check(`${label}: queue track is tappable`, await tapMobile('.queue-row .q-main'));
  check(`${label}: queue selection plays`, !!queueTitle && await waitUntil(`document.querySelector('#now-title')?.textContent === ${JSON.stringify(queueTitle)}`, 2000) < Infinity);
  check(`${label}: queue closes`, await tapMobile('.queue-head-actions button:last-child'));
  check(`${label}: queue returns focus`, await js(`document.activeElement?.getAttribute('aria-label') === 'Queue'`));
  check(`${label}: now playing expands`, await tapMobile('.now-title-button'));
  await mobileScreenshot(`${width}x${height}-now-playing`);
  check(`${label}: full playback controls work`, await tapMobile('.transport-buttons .play'));
  check(`${label}: output selector is visible`, await js(`(() => {const el=document.querySelector('.player-switch-btn'); const r=el.getBoundingClientRect(); return r.width > 0 && r.top >= 0 && r.bottom <= innerHeight && el.contains(document.elementFromPoint(r.x+r.width/2, r.y+r.height/2));})()`));
  check(`${label}: output preset is tappable`, await tapMobile('.output-btn:last-child', true));
  check(`${label}: output preset applies volume`, await waitUntil(`Number(document.querySelector('.transport-aux input[aria-label="Volume"]')?.value) === 20`, 2000) < Infinity);
  await tapMobile('.output-btn:first-child', true);
  await js('document.querySelector(".right-rail").scrollTop = 0');
  check(`${label}: now playing closes`, await tapMobile('[aria-label="Close Now Playing"]'));
  check(`${label}: navigation opens`, await tapMobile('[aria-label="Open navigation"]'));
  check(`${label}: covered content cannot take focus`, await js(`document.querySelector('.content').inert && document.querySelector('.right-rail').inert`));
  check(`${label}: saved playlist is tappable`, await tapMobile('.playlist-list .nav-link', true));
  check(`${label}: saved playlist opens and closes navigation`, await waitUntil(`document.querySelector('.hero-title')?.textContent === 'Phone playlist' && document.querySelector('.hero-sub')?.textContent === '2 tracks' && !document.querySelector('.shell').classList.contains('nav-open')`, 2000) < Infinity);
  await tapMobile('[aria-label="Open navigation"]');
  check(`${label}: navigation closes`, await tapMobile('[aria-label="Close navigation"]'));
  check(`${label}: no horizontal overflow`, await js('document.documentElement.scrollWidth <= innerWidth'));
}

// A remote output choice survives reopening the controller; a removed output
// falls back safely instead of attempting a WebSocket to a nonexistent target.
await tapMobile('.mobile-output');
const chosenOutput = await js(`(() => {
  const select = document.querySelector('.player-switch-btn');
  select.value = [...select.options].find(o => o.value.startsWith('zone:')).value;
  select.dispatchEvent(new Event('change', {bubbles: true}));
  return select.value;
})()`);
await send("Page.reload");
check("mobile: chosen output survives reload", await waitUntil(`document.querySelector('.player-switch-btn')?.value === ${JSON.stringify(chosenOutput)}`, 5000) < Infinity);
check("mobile: main screen names the chosen output", await js(`document.querySelector('.mobile-output strong')?.textContent === document.querySelector('.player-switch-btn option:checked')?.textContent.replace('Zone · ', '')`));
await js(`localStorage.setItem('musicata.output', 'player:removed-output')`);
await send("Page.reload");
check("mobile: removed output falls back to browser", await waitUntil(`document.querySelector('.mobile-output strong')?.textContent === 'This Browser'`, 5000) < Infinity);

// Fresh phone entry and one coherent browser/visible Back history.
await send("Emulation.setDeviceMetricsOverride", {width: 360, height: 800, deviceScaleFactor: 1, mobile: true});
await js(`localStorage.removeItem('musicata.browse-view'); history.replaceState(null, '')`);
await send("Page.reload");
await sleep(1800);
await mobileScreenshot('phone-album-home');
check("phone home opens Albums", await js(`document.querySelector('.seg.is-active')?.textContent.trim() === 'Albums'`));
check("phone has Library, Playlists and Now Playing destinations", await js(`['Library','Playlists','Now Playing'].every(label => [...document.querySelectorAll('.mobile-tabs button')].some(b => b.textContent.trim() === label))`));
await clickText('.seg', 'Albums');
await sleep(500);
await js('window.scrollTo(0, 240)');
const gridScroll = await js('window.scrollY');
await js("document.querySelector('.album-card .card-cover')?.click()");
await sleep(700);
const historyAlbum = await js("document.querySelector('.hero-title')?.textContent");
check("phone title opens Now Playing", await tapMobile('.now-title-button'));
await mobileScreenshot('phone-now-playing-queue');
check("Now Playing includes current queue", await js(`document.querySelector('.shell').classList.contains('np-open') && !!document.querySelector('.queue-drawer.embedded')`));
const embeddedTitle = await js(`document.querySelector('.queue-drawer.embedded .q-title')?.textContent`);
check("Now Playing queue track can be tapped", await tapMobile('.queue-drawer.embedded .q-main', true));
check("Now Playing queue selection plays", !!embeddedTitle && await waitUntil(`document.querySelector('#now-title')?.textContent === ${JSON.stringify(embeddedTitle)}`, 1500) < Infinity);
await js('document.querySelector(".right-rail").scrollTop = 0');
await js('history.back()');
await sleep(500);
check("phone Back closes Now Playing before leaving album", await js(`!document.querySelector('.shell').classList.contains('np-open') && document.querySelector('.hero-title')?.textContent === ${JSON.stringify(historyAlbum)}`));
await js('history.forward()');
await sleep(500);
check("phone Forward restores Now Playing", await js(`document.querySelector('.shell').classList.contains('np-open')`));
await tapMobile('[aria-label="Close Now Playing"]');
await sleep(300);
await tapMobile('.mobile-back');
await sleep(700);
check("visible Back returns to album grid", await js(`document.querySelector('.seg.is-active')?.textContent.trim() === 'Albums' && !document.querySelector('.hero-title')`));
check("Back restores grid scroll", await js(`Math.abs(window.scrollY - ${gridScroll}) < 4`));
await js('history.forward()');
await sleep(600);
check("Forward after visible Back restores album", await js(`document.querySelector('.hero-title')?.textContent === ${JSON.stringify(historyAlbum)}`));
check("phone Playlists destination is reachable", await tapMobile('.mobile-tabs [data-tab="playlists"]'));
check("phone Playlists lists saved playlist", await waitUntil(`document.querySelector('.saved-playlists')?.textContent.includes('Phone playlist')`, 1500) < Infinity);

check("mobile: no uncaught exceptions", exceptions.length === 0, exceptions.slice(0, 3).join(" | "));

check("no CSP violations", cspViolations.length === 0, cspViolations.slice(0, 3).join(" | "));
console.log(failures ? `\nFAILED: ${failures} check(s)` : `\nAll checks passed`);
ws.close();
process.exit(failures ? 1 : 0);
