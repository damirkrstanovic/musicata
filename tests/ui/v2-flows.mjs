// Hot-path + flow smoke for the Svelte app (the old run.mjs/instrument.mjs wrap globals the
// Svelte app doesn't have). Assumes a server with a scanned testdata library is running and
// a headless Chrome with CDP is on :9222. Args: <port> [basePath=/v2]. Exits non-zero on any
// failed assertion.
const PORT = process.argv[2];
const PATH = process.argv[3] || "/v2";
const base = `http://127.0.0.1:${PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const target = await (
  await fetch("http://127.0.0.1:9222/json/new?" + encodeURIComponent(base + PATH), { method: "PUT" })
).json();
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0;
const pending = new Map();
const consoleErrors = [];
ws.addEventListener("message", (e) => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) (pending.get(m.id)(m.result), pending.delete(m.id));
  if (m.method === "Runtime.exceptionThrown")
    consoleErrors.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text);
});
const send = (method, params = {}) =>
  new Promise((res) => (pending.set(++id, res), ws.send(JSON.stringify({ id, method, params }))));
const js = async (expr) =>
  (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result?.value;
const clickText = (sel, text) =>
  js(`[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim()===${JSON.stringify(text)})?.click()`);

let failures = 0;
function check(name, ok, detail = "") {
  console.log(`  ${ok ? "✓" : "✗"} ${name}${ok ? "" : "  <-- " + detail}`);
  if (!ok) failures++;
}

await send("Runtime.enable");
await send("Page.enable");
await sleep(2500);

// --- Hot path: a progress tick must move only the elapsed text, never now-title. ---
await js(`document.querySelector('.album-card')?.click()`);
await sleep(1000);
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
const timeMut = await js(`window.__t`);
const titleMut = await js(`window.__n`);
const playing = await js(`document.querySelector('.transport')?.dataset.status`);
check("playback started", playing === "playing", `status=${playing}`);
check("hot path: elapsed text updates on ticks", timeMut >= 2, `timeMut=${timeMut}`);
check("hot path: now-title NOT swept on ticks", titleMut === 0, `titleMut=${titleMut}`);

// --- Queue drawer ---
await js(`document.querySelector('.queue-btn')?.click()`);
await sleep(500);
check("queue drawer lists tracks", (await js(`document.querySelectorAll('.queue-row').length`)) > 0);
await clickText(".queue-head button", "Close");

// --- Browse filter narrows the grid ---
await clickText(".tab", "Albums");
await sleep(500);
const all = await js(`document.querySelectorAll('.album-card').length`);
await js(`(()=>{const s=document.querySelector('.browse-filters select'); if(s&&s.options.length>1){s.value=s.options[1].value; s.dispatchEvent(new Event('change',{bubbles:true}));}})()`);
await sleep(1000);
const filtered = await js(`document.querySelectorAll('.album-card').length`);
check("browse filter changes the grid", filtered > 0 && filtered <= all, `all=${all} filtered=${filtered}`);
await clickText("button", "Clear");

// --- Search ---
await js(`(()=>{const el=document.querySelector('.search input'); el.value='dar'; el.dispatchEvent(new Event('input',{bubbles:true}));})()`);
await sleep(900);
check("search renders sections", (await js(`document.querySelectorAll('.section-title').length`)) > 0);
await js(`(()=>{const el=document.querySelector('.search input'); el.value=''; el.dispatchEvent(new Event('input',{bubbles:true}));})()`);

// --- Smart playlist opens + lists tracks ---
await clickText(".tab", "Playlists");
await sleep(600);
await js(`[...document.querySelectorAll('.admin-row .admin-row-main')].find(b=>/never played/i.test(b.textContent))?.click()`);
await sleep(900);
check("smart playlist opens + lists tracks", (await js(`document.querySelectorAll('.track-list .track').length`)) > 0);

// --- Metadata editor ---
await js(`document.querySelector('.track-meta')?.click()`);
await sleep(900);
check("metadata panel opens", await js(`!!document.querySelector('.metadata-drawer')`));
await clickText(".queue-head button", "Close");

check("no uncaught exceptions", consoleErrors.length === 0, consoleErrors.slice(0, 3).join(" | "));

console.log(failures ? `\nFAILED: ${failures} check(s)` : `\nAll checks passed`);
ws.close();
process.exit(failures ? 1 : 0);
