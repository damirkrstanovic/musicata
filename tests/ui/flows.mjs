// User-flow definitions for the headless smoke suite, split into two phases:
//
//   behaviorFlows — correctness + responsiveness of the controller, run against the
//     light testdata fixture (reliable, fast). These pin the regressions: footer
//     latency, single broadcast, no audio restart, no per-tick DOM work.
//   scaleFlows    — render/scroll/load behavior at realistic scale, run against a copy
//     of the real ~11k-track library. No playback (heavy page + headless connection
//     limits make real streaming flaky and it isn't what these measure).
//
// Each flow returns { name, checks: [{ label, ok, info }], skipped? }. `ctx` is
// provided by run.mjs: { eval, sleep, snapshot, reset, mark, log, budgets, loadMs }.

const check = (label, ok, info = "") => ({ label, ok: !!ok, info: String(info) });

// Latency (ms) from the flow's FLOW marker to the first footer title matching `text`.
function footerLatency(log, text) {
  const start = log.find((e) => e.kind === "FLOW");
  const hit = log.find((e) => e.kind === "footerTitle" && e.text === text);
  return start && hit ? hit.t - start.t : null;
}

const titleSeen = (log, text) => log.some((e) => e.kind === "footerTitle" && e.text === text);

export async function behaviorFlows(ctx) {
  const { eval: ev, fire, sleep, snapshot, reset, mark, budgets, log = () => {} } = ctx;
  const results = [];

  // Setup: use the library the app already loaded; drive playback from the streamable
  // (local-disk) subset so audio lifecycle events fire.
  const setup = await ev(`(() => {
    const all = (state.tracks || []).map(t => ({ id: t.id, title: t.title,
      stream_url: t.stream_url || ('/api/tracks/' + t.id + '/stream'),
      provider: (t.provider && t.provider.provider_id) || null }));
    const playable = all.filter(t => t.provider === 'local-disk');
    state.visibleTracks = playable.length ? playable : all;
    if (browserPlayerId) setActivePlayer(browserPlayerId);
    return { total: all.length, playable: playable.length,
      titles: state.visibleTracks.map(t=>t.title).slice(0, 8),
      players: (playerData.players||[]).length, browserPlayerId };
  })()`);
  const titles = setup.titles || [];

  results.push({
    name: "initial load",
    checks: [
      check("library loaded", setup.total > 0, `${setup.total} tracks`),
      check("browser player active", setup.browserPlayerId === "browser-local", setup.browserPlayerId),
      check("no console errors", (await ev(`window.__console.length`)) === 0),
    ],
  });

  if ((setup.playable || 0) < 3) {
    results.push({ name: "playback flows", skipped: true, reason: "need >=3 streamable tracks" });
    return results;
  }

  // Play the FIRST track.
  await reset(); await mark("playTrack(0)"); log("playTrack(0)");
  await fire(`playTrack(0)`);
  await sleep(1200);
  let snap = await snapshot();
  {
    const lat = footerLatency(snap.log, titles[0]);
    const full = snap.log.filter((e) => e.kind === "fullState" && e.now);
    results.push({
      name: "play first track",
      checks: [
        check("footer shows the track", titleSeen(snap.log, titles[0]), titles[0]),
        check(`footer updates within ${budgets.footerMs}ms`, lat != null && lat <= budgets.footerMs, `${lat}ms`),
        check("exactly one full-state broadcast", full.length === 1, `${full.length}`),
        check("audio not restarted", snap.counts.srcReset === 0, `srcReset=${snap.counts.srcReset}`),
      ],
    });
  }

  // Play a LATER track — the double-play / footer-lag regression.
  await reset(); await mark("playTrack(2)"); log("playTrack(2)");
  await fire(`playTrack(2)`);
  await sleep(1200);
  snap = await snapshot();
  {
    const lat = footerLatency(snap.log, titles[2]);
    const full = snap.log.filter((e) => e.kind === "fullState" && e.now);
    results.push({
      name: "play later track (index 2)",
      checks: [
        check("footer shows the CLICKED track", titleSeen(snap.log, titles[2]), titles[2]),
        check("never flashed the first track", !titleSeen(snap.log, titles[0]) || titles[0] === titles[2]),
        check(`footer updates within ${budgets.footerMs}ms`, lat != null && lat <= budgets.footerMs, `${lat}ms`),
        check("single broadcast (no 0-then-N)", full.length === 1, `${full.length}`),
        check("audio not restarted", snap.counts.srcReset === 0, `srcReset=${snap.counts.srcReset}`),
      ],
    });
  }

  // Steady playback: only lightweight ticks, no heavy re-render.
  await reset(); log("steady playback");
  await sleep(3000);
  snap = await snapshot();
  results.push({
    name: "steady playback (3s)",
    checks: [
      check("position ticks flowing", snap.counts.progressTick >= 2, `${snap.counts.progressTick} ticks`),
      check("no full-state between track changes", (snap.counts.fullState || 0) === 0, `fullState=${snap.counts.fullState}`),
      check("no track-list re-highlight on ticks", (snap.counts.markActiveTrack || 0) === 0, `${snap.counts.markActiveTrack}`),
      check("no queue rebuild on ticks", (snap.counts.renderQueue || 0) === 0, `${snap.counts.renderQueue}`),
      check("no media-metadata rebuild on ticks", (snap.counts.msMeta || 0) === 0, `${snap.counts.msMeta}`),
    ],
  });

  // Pause then resume — no restart.
  await reset(); log("pause/resume");
  await fire(`document.querySelector('#play-pause').click()`);
  await sleep(600);
  await fire(`document.querySelector('#play-pause').click()`);
  await sleep(600);
  snap = await snapshot();
  results.push({
    name: "pause / resume",
    checks: [
      check("audio paused", snap.log.some((e) => e.kind === "audio:pause")),
      check("audio resumed", snap.log.some((e, i) => e.kind === "audio:play" && snap.log.slice(0, i).some((p) => p.kind === "audio:pause"))),
      check("no src reset on pause/resume", snap.counts.srcReset === 0, `srcReset=${snap.counts.srcReset}`),
    ],
  });

  // Next — a real track change (a src change here IS expected).
  await reset(); await mark("next"); log("next");
  await fire(`document.querySelector('#next').click()`);
  await sleep(1000);
  snap = await snapshot();
  results.push({
    name: "next track",
    checks: [
      check("footer updated", snap.log.some((e) => e.kind === "footerTitle")),
      check("single full-state broadcast", snap.log.filter((e) => e.kind === "fullState" && e.now).length === 1, `${snap.log.filter((e) => e.kind === "fullState" && e.now).length}`),
    ],
  });

  // Seek.
  await reset(); log("seek");
  await fire(`playerCommand(state.activePlayerId, { command: 'seek', position_seconds: 42 })`);
  await sleep(700);
  results.push({
    name: "seek",
    checks: [check("elapsed jumped to ~42s", Math.abs(((await ev(`state.activeElapsed`)) ?? 0) - 42) <= 3, `elapsed=${await ev(`state.activeElapsed`)}`)],
  });

  // Queue drawer open + reorder — no restart, renders only on change.
  await reset(); log("queue reorder");
  await fire(`document.querySelector('#queue-toggle').click()`);
  await sleep(300);
  const before = await ev(`(state.activeState.queue||[]).map(q=>q.track_id||q.title)`);
  await fire(`playerCommand(state.activePlayerId, { command: 'move_queue_item', from: 0, to: 1 })`);
  await sleep(700);
  snap = await snapshot();
  {
    const after = await ev(`(state.activeState.queue||[]).map(q=>q.track_id||q.title)`);
    results.push({
      name: "queue drawer reorder",
      checks: [
        check("queue order changed", JSON.stringify(before) !== JSON.stringify(after)),
        check("queue re-rendered on the change", (snap.counts.renderQueue || 0) >= 1, `${snap.counts.renderQueue}`),
        check("no audio restart on reorder", snap.counts.srcReset === 0, `srcReset=${snap.counts.srcReset}`),
      ],
    });
    await fire(`document.querySelector('#queue-toggle').click()`);
  }

  // Internet radio — browse via the provider, then play the stream.
  await reset(); log("radio");
  const radio = await ev(`(async () => {
    await fetch('/api/radio', { method:'POST', headers:{'content-type':'application/json'},
      body: JSON.stringify({ name:'Smoke FM', stream_url: location.origin + '/api/tracks/' + state.visibleTracks[0].id + '/stream' }) });
    const r = await fetch('/api/sources/radio/browse'); const j = await r.json();
    const station = (j.entries||[]).find(e => e.title === 'Smoke FM');
    if (!station) return { ok:false };
    playRadio({ name: station.title, stream_url: station.stream_url });
    return { ok:true };
  })()`);
  await sleep(1000);
  snap = await snapshot();
  results.push({
    name: "internet radio",
    checks: [
      check("station browsed via provider + played", radio && radio.ok),
      check("now-playing reflects the stream", titleSeen(snap.log, "Smoke FM") || (await ev(`state.activeState?.now_playing?.title`)) === "Smoke FM"),
      check("no console errors", snap.console.length === 0, snap.console.join("; ")),
    ],
  });

  return results;
}

export async function scaleFlows(ctx) {
  const { eval: ev, sleep, snapshot, reset, budgets, loadMs } = ctx;
  const results = [];

  const info = await ev(`({ total: (state.tracks||[]).length, rows: document.querySelectorAll('#track-list .track').length })`);

  results.push({
    name: "initial load at scale",
    checks: [
      check("realistic library size", info.total >= 1000, `${info.total} tracks`),
      check("full list rendered", info.rows >= Math.min(info.total, 1000), `${info.rows} / ${info.total} rows`),
      check("app interactive within 20s", (loadMs ?? 0) < 20000, `${loadMs}ms to first interactive`),
      check("no console errors on load", (await ev(`window.__console.length`)) === 0),
    ],
  });

  // Scroll the full library top-to-bottom; content-visibility should keep the main
  // thread free of long tasks.
  await reset();
  await ev(`(() => {
    const el = document.querySelector('#track-list') || document.scrollingElement;
    const h = el.scrollHeight; const step = Math.max(400, h / 50);
    for (let y = 0; y <= h; y += step) el.scrollTop = y;
    el.scrollTop = 0;
  })()`);
  await sleep(800);
  const snap = await snapshot();
  const long = snap.log.filter((e) => e.kind === "longtask" && e.dur > budgets.longTaskMs);
  results.push({
    name: `library scroll (${info.rows} rows)`,
    checks: [check(`no scroll long task > ${budgets.longTaskMs}ms`, long.length === 0, long.map((l) => l.dur + "ms").join(", "))],
  });

  return results;
}
