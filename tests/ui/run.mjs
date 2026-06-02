#!/usr/bin/env node
// Headless UI + lag smoke suite. Drives the real web app in headless Chromium over the
// DevTools protocol (no Playwright; uses Node's built-in fetch + WebSocket) in two
// phases:
//
//   1. behavior — playback/controller correctness + responsiveness, on the light
//      testdata fixture (reliable, fast). Pins the regressions.
//   2. scale    — render/scroll/load at realistic scale, on a read-only copy of the
//      real ~11k-track library (skipped if that DB isn't present).
//
// Exits non-zero if any check fails. Skips (exit 0) if no Chromium is installed.
//
// Usage:  node tests/ui/run.mjs   (or: scripts/ui-smoke.sh)
// Env:    MUSICATA_BIN, MUSICATA_LIBRARY, MUSICATA_REAL_DB, CHROMIUM, UI_PORT,
//         UI_DEBUG_PORT, UI_VERBOSE, UI_EVLOG

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, copyFileSync, appendFileSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join } from "node:path";
import { behaviorFlows, scaleFlows } from "./flows.mjs";
import { INSTRUMENT } from "./instrument.mjs";

const ROOT = join(import.meta.dirname, "..", "..");
const CFG = {
  bin: process.env.MUSICATA_BIN || join(ROOT, "target", "debug", "musicata-server"),
  library: process.env.MUSICATA_LIBRARY || join(ROOT, "testdata"),
  realDb: process.env.MUSICATA_REAL_DB || join(ROOT, ".musicata", "musicata.db"),
  chromium: process.env.CHROMIUM ||
    join(homedir(), ".cache/ms-playwright/chromium-1217/chrome-linux64/chrome"),
  host: "127.0.0.1",
  port: Number(process.env.UI_PORT || 3939),
  debugPort: Number(process.env.UI_DEBUG_PORT || 9333),
  // Generous budgets — detect outliers on a dev machine, not micro-regressions.
  budgets: { footerMs: 1500, longTaskMs: 400 },
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// Synchronous heartbeat (survives a SIGKILL where buffered stdout would be lost).
const beat = (m) => { if (process.env.UI_HEARTBEAT) try { appendFileSync(process.env.UI_HEARTBEAT, `${Date.now()} ${m}\n`); } catch {} };

async function waitFor(label, fn, { tries = 100, every = 100 } = {}) {
  for (let i = 0; i < tries; i++) {
    try { if (await fn()) return true; } catch { /* retry */ }
    await sleep(every);
  }
  throw new Error(`timed out waiting for ${label}`);
}

// ---- CDP: a flat-session client over the browser WebSocket. ----
async function connectCdp(debugPort) {
  const ver = await (await fetch(`http://127.0.0.1:${debugPort}/json/version`)).json();
  const ws = new WebSocket(ver.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); }
  };
  const send = (method, params = {}, sessionId) =>
    new Promise((res) => { const mid = ++id; pending.set(mid, res); ws.send(JSON.stringify({ id: mid, method, params, sessionId })); });
  return { ws, send };
}

async function main() {
  if (!existsSync(CFG.bin)) {
    console.error(`✗ server binary not found at ${CFG.bin}\n  build it first: cargo build -p musicata-server`);
    process.exit(2);
  }
  if (!existsSync(CFG.chromium)) {
    console.log(`SKIP: no Chromium at ${CFG.chromium} (set $CHROMIUM). UI smoke suite skipped.`);
    process.exit(0);
  }

  const tmp = mkdtempSync(join(tmpdir(), "musicata-ui-"));
  const procs = [];
  let cleaned = false;
  const cleanup = () => {
    if (cleaned) return;
    cleaned = true;
    for (const p of procs) { try { p.kill("SIGKILL"); } catch {} }
    try { rmSync(tmp, { recursive: true, force: true }); } catch {}
  };
  process.on("exit", cleanup);
  // A bare `timeout`/SIGTERM kill must NOT orphan the server + Chromium (orphans hold
  // the ports and break the next run), so clean up on signals too.
  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) process.on(sig, () => { cleanup(); process.exit(2); });

  // Chromium (one instance, a fresh page per phase).
  const chrome = spawn(CFG.chromium, [
    "--headless=new", "--no-sandbox", "--disable-gpu", "--mute-audio",
    "--autoplay-policy=no-user-gesture-required",
    `--remote-debugging-port=${CFG.debugPort}`, `--user-data-dir=${join(tmp, "profile")}`, "about:blank",
  ], { stdio: ["ignore", "pipe", "pipe"] });
  procs.push(chrome);
  await waitFor("chromium devtools", async () => (await fetch(`http://127.0.0.1:${CFG.debugPort}/json/version`)).ok);
  const { send } = await connectCdp(CFG.debugPort);

  // Watchdog: a pre-commit gate must never hang.
  const watchdog = setTimeout(() => { console.error("✗ watchdog: suite exceeded 150s — aborting"); process.exit(2); }, 150_000);

  const makeEv = (sid) => async (expr) => {
    if (process.env.UI_EVLOG) process.stderr.write(`    eval> ${expr.slice(0, 70).replace(/\n/g, " ")}\n`);
    // Per-eval timeout: a single stalled evaluate (e.g. a fetch starved by the page)
    // must surface as a failed check, never hang the whole suite.
    const evaluate = send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true }, sid);
    const timeout = new Promise((_, rej) => setTimeout(() => rej(new Error(`eval timed out (15s): ${expr.slice(0, 50).replace(/\n/g, " ")}`)), 15_000));
    const { result } = await Promise.race([evaluate, timeout]);
    if (result?.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description || "eval threw");
    return result?.result?.value;
  };

  // Run one phase: its own server + DB on `port`, a fresh page, instrument, flows.
  async function runPhase({ port, useRealDb, flowsFn, label }) {
    const dbPath = join(tmp, `${label}.db`);
    const args = ["--library", CFG.library, "--addr", `${CFG.host}:${port}`, "--database", dbPath];
    let dataset = "testdata (scanned)";
    if (useRealDb) {
      if (!existsSync(CFG.realDb)) return { dataset: "real DB (absent)", results: [{ name: "scale flows", skipped: true, reason: `no ${CFG.realDb}` }] };
      copyFileSync(CFG.realDb, dbPath);
      for (const sfx of ["-wal", "-shm"]) if (existsSync(CFG.realDb + sfx)) copyFileSync(CFG.realDb + sfx, dbPath + sfx);
      args.push("--no-scan");
      dataset = "real library copy (--no-scan)";
    }
    const base = `http://${CFG.host}:${port}`;
    const server = spawn(CFG.bin, args, { stdio: ["ignore", "pipe", "pipe"] });
    procs.push(server);
    await waitFor(`${label} server health`, async () => (await fetch(`${base}/api/health`)).ok);

    const { result: target } = await send("Target.createTarget", { url: "about:blank" });
    const { result: attach } = await send("Target.attachToTarget", { targetId: target.targetId, flatten: true });
    const sid = attach.sessionId;
    await send("Runtime.enable", {}, sid);
    await send("Page.enable", {}, sid);
    // A CDP-created tab has no fixed layout viewport, so `100vh` resolves to the content
    // height and the app's internal scrollers (the .content panel, the sidebar) never
    // engage — breaking scroll-driven behavior like infinite scroll. Pin a real viewport.
    await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false }, sid);
    await send("Page.navigate", { url: base + "/" }, sid);
    const ev = makeEv(sid);
    // Fire an action without waiting for its returned promise (e.g. playTrack/
    // playerCommand kick off a fetch; the test observes the resulting WS state, it
    // doesn't need the action's own promise to settle). Returns as soon as the call is
    // dispatched, so a slow/in-flight fetch can't stall the suite.
    const fire = async (expr) => {
      if (process.env.UI_EVLOG) process.stderr.write(`    fire> ${expr.slice(0, 70).replace(/\n/g, " ")}\n`);
      await send("Runtime.evaluate", { expression: `void (${expr})`, returnByValue: true, awaitPromise: false }, sid);
    };

    const loadStart = Date.now();
    await waitFor(`${label} app ready`, async () =>
      (await ev(`!!(typeof state!=='undefined' && typeof playerData!=='undefined' && (playerData.players||[]).length && (state.tracks||[]).length)`)) === true,
      { tries: 400, every: 100 });
    const loadMs = Date.now() - loadStart;

    const inst = await ev(INSTRUMENT);
    if (inst !== "instrumented" && inst !== "already") throw new Error("instrumentation failed");

    const ctx = {
      eval: ev, sleep,
      snapshot: () => ev(`({ log: window.__log, counts: window.__counts, console: window.__console })`),
      reset: () => ev(`window.__reset()`),
      mark: (name) => ev(`window.__push('FLOW', { name: ${JSON.stringify(name)} })`),
      fire,
      log: (m) => { beat(`${label}: ${m}`); if (process.env.UI_VERBOSE) process.stderr.write(`  … ${m}\n`); },
      budgets: CFG.budgets, loadMs, base,
    };
    const results = await flowsFn(ctx);
    try { await send("Target.closeTarget", { targetId: target.targetId }); } catch {}
    server.kill("SIGKILL");
    return { dataset, results };
  }

  const phases = [];
  beat("behavior phase start");
  phases.push({ title: "behavior", ...(await runPhase({ port: CFG.port, useRealDb: false, flowsFn: behaviorFlows, label: "behavior" })) });
  beat("behavior phase done");
  beat("scale phase start");
  phases.push({ title: "scale", ...(await runPhase({ port: CFG.port + 1, useRealDb: true, flowsFn: scaleFlows, label: "scale" })) });
  beat("scale phase done");
  clearTimeout(watchdog);

  // ---- Report ----
  let failed = 0, passed = 0, skipped = 0;
  console.log(`\nMusicata UI/lag smoke suite\n`);
  for (const phase of phases) {
    console.log(`▌ ${phase.title} phase — ${phase.dataset}`);
    for (const flow of phase.results) {
      if (flow.skipped) { console.log(`  ⦰ ${flow.name} — SKIPPED (${flow.reason})`); skipped++; continue; }
      const bad = flow.checks.filter((c) => !c.ok);
      console.log(`  ${bad.length ? "✗" : "✓"} ${flow.name}`);
      for (const c of flow.checks) {
        if (c.ok) passed++; else failed++;
        if (!c.ok || process.env.UI_VERBOSE) console.log(`      ${c.ok ? "✓" : "✗"} ${c.label}${c.info ? "  (" + c.info + ")" : ""}`);
      }
    }
  }
  console.log(`\n${passed} passed, ${failed} failed, ${skipped} flow(s) skipped\n`);
  process.exit(failed ? 1 : 0);
}

main().catch((e) => { console.error("harness error:", e.message); process.exit(2); });
