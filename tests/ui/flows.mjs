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

  // Zones — a zone owns a canonical, server-side queue and is a control target like
  // a player. Put the browser player in a zone, select the zone, and play: the same
  // single-broadcast / no-restart / ticks-flowing guarantees must hold off the zone
  // socket, and a reorder must run against /api/zones/{id}/commands.
  await reset(); log("zone setup");
  const zoneSetup = await ev(`(async () => {
    const z = await (await fetch('/api/zones', { method:'POST', headers:{'content-type':'application/json'},
      body: JSON.stringify({ name: 'Smoke Zone' }) })).json();
    await fetch('/api/players/' + encodeURIComponent(browserPlayerId), { method:'PATCH',
      headers:{'content-type':'application/json'}, body: JSON.stringify({ zone_id: z.id }) });
    await loadPlayers();                 // pick up the new zone + browser membership
    setActivePlayer(z.id);
    return { zoneId: z.id, isZone: isZoneTarget(z.id), outputs: browserOutputsFor(z.id) };
  })()`);
  await sleep(400);                       // let the zone socket connect

  await reset(); await mark("zone playTrack(0)"); log("zone playTrack(0)");
  await fire(`playTrack(0)`);
  await sleep(1400);
  snap = await snapshot();
  {
    const lat = footerLatency(snap.log, titles[0]);
    const full = snap.log.filter((e) => e.kind === "fullState" && e.now);
    results.push({
      name: "zone owns queue + plays",
      checks: [
        check("zone is a control target", zoneSetup.isZone, String(zoneSetup.isZone)),
        check("browser outputs for the zone", zoneSetup.outputs, String(zoneSetup.outputs)),
        check("footer shows the track", titleSeen(snap.log, titles[0]), titles[0]),
        check(`footer updates within ${budgets.footerMs}ms`, lat != null && lat <= budgets.footerMs, `${lat}ms`),
        check("exactly one full-state broadcast", full.length === 1, `${full.length}`),
        check("audio not restarted", snap.counts.srcReset === 0, `srcReset=${snap.counts.srcReset}`),
      ],
    });
  }

  // Steady zone playback: position ticks flow over the zone socket.
  await reset(); log("zone steady");
  await sleep(2500);
  snap = await snapshot();
  results.push({
    name: "zone steady playback",
    checks: [
      check("position ticks flowing", snap.counts.progressTick >= 2, `${snap.counts.progressTick} ticks`),
      check("no queue rebuild on ticks", (snap.counts.renderQueue || 0) === 0, `${snap.counts.renderQueue}`),
    ],
  });

  // Reorder the zone's queue — routed to /api/zones/{id}/commands via commandTarget.
  await reset(); log("zone reorder");
  await fire(`document.querySelector('#queue-toggle').click()`);
  await sleep(300);
  const zBefore = await ev(`(state.activeState.queue||[]).map(q=>q.track_id||q.title)`);
  await fire(`commandTarget({ command: 'move_queue_item', from: 0, to: 1 })`);
  await sleep(700);
  snap = await snapshot();
  {
    const zAfter = await ev(`(state.activeState.queue||[]).map(q=>q.track_id||q.title)`);
    results.push({
      name: "zone queue reorder",
      checks: [
        check("queue order changed", JSON.stringify(zBefore) !== JSON.stringify(zAfter)),
        check("queue re-rendered on the change", (snap.counts.renderQueue || 0) >= 1, `${snap.counts.renderQueue}`),
        check("no audio restart on reorder", snap.counts.srcReset === 0, `srcReset=${snap.counts.srcReset}`),
      ],
    });
    await fire(`document.querySelector('#queue-toggle').click()`);
  }

  // Cleanup: leave the browser player active and remove the smoke zone.
  await ev(`(async () => {
    setActivePlayer(browserPlayerId);
    await fetch('/api/zones/' + encodeURIComponent(${JSON.stringify(zoneSetup.zoneId)}), { method:'DELETE' });
    await loadPlayers();
  })()`);

  return results;
}

export async function scaleFlows(ctx) {
  const { eval: ev, sleep, snapshot, reset, budgets, loadMs, base } = ctx;
  const results = [];

  // Library size comes from the already-loaded summary signature ("tracks:albums"),
  // NOT state.tracks — once tracks page in on scroll, state.tracks holds only the
  // loaded window. (Reading state avoids a fresh fetch racing the settling load.)
  const info = await ev(`(() => {
    const [total, albums] = (state.librarySignature || '0:0').split(':').map(Number);
    // The fix is "never pull the whole track table up front." The deterministic signal
    // is the size of the largest /api/tracks response: the old eager load fetched the
    // entire library in one ~8 MB response; now it's one bounded page. (A total-bytes
    // metric would be flaky — it races progressively-loading cover <img>s and media art.)
    const sz = (r) => r.encodedBodySize || r.transferSize || 0;
    const tracksFetchKB = performance.getEntriesByType('resource')
      .filter(r => /\\/api\\/tracks\\?/.test(r.name) && (r.initiatorType === 'fetch' || r.initiatorType === 'xmlhttprequest'))
      .reduce((m, r) => Math.max(m, sz(r)), 0) / 1024;
    return {
      total, albums,
      rows: document.querySelectorAll('#track-list .track').length,
      loadedTracks: (state.tracks || []).length,
      tracksFetchKB: Math.round(tracksFetchKB),
    };
  })()`);

  // The win: first interactive must NOT wait on the whole library. We assert a
  // windowed initial render and a one-page track fetch. On the old eager load
  // (~8.3 MB tracks, 11k rows) these go red — that's the documented baseline; the
  // paging work turns them green.
  results.push({
    name: "initial load at scale",
    checks: [
      check("realistic library size", info.total >= 1000, `${info.total} tracks, ${info.albums} albums`),
      check("initial render is windowed (<= 400 rows)", info.rows <= 400, `${info.rows} rows for ${info.total} tracks`),
      check("tracks load one page, not the whole library", info.tracksFetchKB <= 200, `${info.tracksFetchKB} KB (was ~8.3 MB eager)`),
      check("app interactive within 3s", (loadMs ?? 0) < 3000, `${loadMs}ms to first interactive`),
      check("no console errors on load", (await ev(`window.__console.length`)) === 0),
    ],
  });

  // Infinite scroll: reaching the end of the list pulls and appends the next page,
  // with no explicit page UI. Scroll to the bottom and confirm more rows appear.
  await reset();
  const before = await ev(`document.querySelectorAll('#track-list .track').length`);
  let after = before;
  for (let i = 0; i < 6 && after <= before; i++) {
    // The scroller is the .content panel, not #track-list itself.
    await ev(`(() => {
      const el = document.querySelector('.content') || document.scrollingElement;
      el.scrollTop = el.scrollHeight;
    })()`);
    await sleep(600);
    after = await ev(`document.querySelectorAll('#track-list .track').length`);
  }
  results.push({
    name: "infinite scroll loads more",
    checks: [
      check("scrolling to the end appends more rows", after > before, `${before} → ${after} rows`),
    ],
  });

  // Scroll the loaded rows; content-visibility should keep the main thread free of
  // long tasks even as pages append.
  await reset();
  await ev(`(() => {
    const el = document.querySelector('.content') || document.scrollingElement;
    const h = el.scrollHeight; const step = Math.max(400, h / 50);
    for (let y = 0; y <= h; y += step) el.scrollTop = y;
    el.scrollTop = 0;
  })()`);
  await sleep(800);
  let snap = await snapshot();
  let long = snap.log.filter((e) => e.kind === "longtask" && e.dur > budgets.longTaskMs);
  results.push({
    name: "library scroll stays smooth",
    checks: [check(`no scroll long task > ${budgets.longTaskMs}ms`, long.length === 0, long.map((l) => l.dur + "ms").join(", "))],
  });

  // Search at scale must return a bounded, fast page (not every match). Measured from
  // Node, not the page — at scale Chromium's 6-connection cap is saturated by the
  // eager album-artwork image storm, so a page-side fetch would queue behind it.
  const t = Date.now();
  const j = await fetch(`${base}/api/search?q=a&limit=100`).then((r) => r.json());
  const searchInfo = { ms: Date.now() - t, tracks: (j.tracks || []).length, albums: (j.albums || []).length };
  results.push({
    name: "search at scale",
    checks: [
      check("search responds < 500ms", searchInfo.ms < 500, `${searchInfo.ms}ms`),
      check("search page is bounded", searchInfo.tracks <= 100, `${searchInfo.tracks} tracks`),
      check("search returns matches", searchInfo.tracks + searchInfo.albums > 0, `${searchInfo.tracks}t/${searchInfo.albums}a`),
    ],
  });

  // The Albums view renders cover cards (with ?size= thumbnails), and a browse facet
  // narrows that grid. Switch to Albums, pick the rarest genre, and confirm the rendered
  // cards are exactly the filtered albums.
  await reset();
  const picked = await ev(`(async () => {
    const totalAlbums = (state.librarySignature || '0:0').split(':').map(Number)[1];
    document.querySelector('#segmented .seg[data-view="albums"]').click();
    await new Promise((r) => setTimeout(r, 400));
    const genres = (state.browse.genres || []).slice().sort((a, b) => a.track_count - b.track_count);
    const g = genres[0];
    if (!g) return { skip: true, gridVisible: !document.querySelector('#browse-grid').hidden };
    els.browseGenre.value = String(g.value);
    await applyBrowseFilter();
    return { genre: String(g.value), totalAlbums };
  })()`);
  if (picked.skip) {
    results.push({ name: "browse filters the album grid", skipped: true, reason: "no genre facets" });
  } else {
    await sleep(700);
    const filtered = await fetch(`${base}/api/albums?genre=${encodeURIComponent(picked.genre)}&limit=500`).then((r) => r.json());
    const ids = new Set(filtered.items.map((a) => a.id));
    const grid = await ev(`(() => ({
      visible: !document.querySelector('#browse-grid').hidden,
      ids: Array.from(document.querySelectorAll('#browse-grid .album-card')).map((e) => e.dataset.albumId),
      sized: Array.from(document.querySelectorAll('#browse-grid .album-card img')).every((i) => i.src.includes('size=300')),
    }))()`);
    results.push({
      name: "browse filters the album grid",
      checks: [
        check("album grid narrowed to the genre", filtered.total > 0 && filtered.total < picked.totalAlbums, `${filtered.total}/${picked.totalAlbums} albums match "${picked.genre}"`),
        check("rendered cards are all in the filtered set", grid.visible && grid.ids.length > 0 && grid.ids.every((id) => ids.has(id)), `${grid.ids.length} cards`),
        check("cards request sized thumbnails", grid.sized, "?size=300"),
      ],
    });
  }

  // Master→detail: open an album from the grid → hero header + tracklist; open its artist
  // → albums grid; Back returns to the album.
  await reset();
  const detail = await ev(`(async () => {
    document.querySelector('#segmented .seg[data-view="albums"]').click();
    await new Promise((r) => setTimeout(r, 600));
    const card = document.querySelector('#browse-grid .album-card');
    if (!card) return { skip: true };
    card.click();
    await new Promise((r) => setTimeout(r, 700));
    const heroShown = !document.querySelector('#detail-hero').hidden && !!document.querySelector('.hero-title');
    const hasTracks = document.querySelectorAll('#track-list .track').length > 0;
    const hasArtistLink = !!document.querySelector('.link-artist');
    const albumTitle = document.querySelector('.hero-title')?.textContent || '';
    document.querySelector('.link-artist')?.click();
    await new Promise((r) => setTimeout(r, 700));
    const artistHero = !document.querySelector('#detail-hero').hidden && document.querySelector('#detail-hero .eyebrow')?.textContent === 'Artist';
    const artistAlbums = document.querySelectorAll('#browse-grid .album-card').length;
    // The artist page is albums-only — the track list must be actually hidden, not just
    // flagged hidden (a CSS display rule once overrode the [hidden] attribute).
    const artistTracksVisible =
      getComputedStyle(document.querySelector('#track-list')).display !== 'none';
    const backShown = !document.querySelector('#back-btn').hidden;
    history.back();
    await new Promise((r) => setTimeout(r, 700));
    const backToAlbum = (document.querySelector('.hero-title')?.textContent || '') === albumTitle;
    return { heroShown, hasTracks, hasArtistLink, artistHero, artistAlbums, artistTracksVisible, backShown, backToAlbum };
  })()`);
  if (detail.skip) {
    results.push({ name: "browse master→detail", skipped: true, reason: "no album cards" });
  } else {
    results.push({
      name: "browse master→detail",
      checks: [
        check("album opens a hero + tracklist", detail.heroShown && detail.hasTracks, `hero=${detail.heroShown} tracks=${detail.hasTracks}`),
        check("artist link opens the artist page", detail.artistHero && detail.artistAlbums > 0, `${detail.artistAlbums} albums`),
        check("artist page hides the track list", !detail.artistTracksVisible, `tracksVisible=${detail.artistTracksVisible}`),
        check("Back returns to the album", detail.backShown && detail.backToAlbum, `back=${detail.backToAlbum}`),
      ],
    });
  }

  // Long artist names must clip to their card, not bleed into the next column. Switch to
  // the Artists grid and check the widest name still fits its card.
  await reset();
  const artistGrid = await ev(`(async () => {
    document.querySelector('#segmented .seg[data-view="artists"]').click();
    await new Promise((r) => setTimeout(r, 700));
    const cards = [...document.querySelectorAll('#browse-grid .artist-card')];
    if (!cards.length) return { skip: true };
    let worst = 0, longest = '';
    for (const card of cards) {
      const s = card.querySelector('.card-text strong');
      if (!s) continue;
      const over = s.getBoundingClientRect().width - card.getBoundingClientRect().width;
      if (over > worst) { worst = over; longest = s.textContent; }
    }
    return { count: cards.length, worst: Math.round(worst), longest };
  })()`);
  if (artistGrid.skip) {
    results.push({ name: "artist cards clip long names", skipped: true, reason: "no artist cards" });
  } else {
    results.push({
      name: "artist cards clip long names",
      checks: [
        check(
          "names stay within the card",
          artistGrid.worst <= 1,
          `worst overflow=${artistGrid.worst}px on "${artistGrid.longest}" across ${artistGrid.count} cards`,
        ),
      ],
    });
  }

  // Destructive actions use an in-product modal, never a native dialog (which is
  // suppressed in PWAs). Create a playlist, delete it, and confirm the modal drives it.
  await reset();
  const modal = await ev(`(async () => {
    const made = await apiJson('/api/playlists', 'POST', { name: 'Smoke Modal PL' });
    await loadPlaylists();
    deletePlaylist(made.id, made.name);
    await new Promise((r) => setTimeout(r, 250));
    const modalShown = !!document.querySelector('.modal-scrim .modal');
    document.querySelector('.modal-scrim .primary-button, .modal-scrim .ghost-button.danger')?.click();
    await new Promise((r) => setTimeout(r, 400));
    const gone = (await api('/api/playlists')).every((p) => p.id !== made.id);
    const closed = !document.querySelector('.modal-scrim');
    return { modalShown, gone, closed };
  })()`);
  results.push({
    name: "delete uses an in-product modal",
    checks: [
      check("a modal appears (no native confirm)", modal.modalShown, `shown=${modal.modalShown}`),
      check("confirming deletes the playlist", modal.gone && modal.closed, `gone=${modal.gone}`),
    ],
  });

  // Smart playlists: a fixed catalog of computed views. With nothing played in the
  // scale library, "Never played" lists tracks. Open it and confirm a master track list
  // renders (and a track is playable through the normal row).
  await reset();
  const smart = await ev(`(async () => {
    await loadSmartPlaylists();
    const sidebarCount = document.querySelectorAll('#smart-playlists .playlist-item').length;
    await openSmartPlaylistView('never-played');
    await new Promise((r) => setTimeout(r, 500));
    const active = document.querySelectorAll('#smart-playlists .playlist-item.is-active').length;
    const title = document.querySelector('#view-title')?.textContent || '';
    const trackCount = document.querySelectorAll('#track-list .track').length;
    return { sidebarCount, active, title, trackCount };
  })()`);
  results.push({
    name: "smart playlist opens and lists tracks",
    checks: [
      check("catalog renders in the sidebar", smart.sidebarCount >= 3, `count=${smart.sidebarCount}`),
      check("the opened entry is highlighted", smart.active === 1, `active=${smart.active}`),
      check(
        "opening never-played lists tracks",
        smart.trackCount > 0,
        `title=${smart.title} tracks=${smart.trackCount}`,
      ),
    ],
  });

  return results;
}
