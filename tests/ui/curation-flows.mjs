// Desktop listening-curation smoke flow. Kept separate from v2-flows so the
// curation journey can evolve without obscuring transport and responsive checks.
export async function curationFlows({ js, api, check, waitUntil, clickText }) {
  const closeQueue = async () => {
    await js('document.querySelector("[aria-label=\\\"Close queue\\\"]")?.click()');
    await waitUntil('!document.querySelector(".queue-drawer")', 1000);
  };
  await clickText('.seg', 'Tracks');
  const tracksReady = await waitUntil('document.querySelectorAll(".track").length >= 2', 3000) < Infinity;
  check('curation: tracks are available', tracksReady);
  if (!tracksReady) return closeQueue();

  // Radio leaves a stream-only queue behind. Clear it so a rendered row cannot make this flow
  // mistake the prior queue for the local-track queue that this control must save.
  await js('if (!document.querySelector(".queue-drawer")) document.querySelector("[aria-label=Queue]")?.click()');
  await waitUntil('document.querySelector(".queue-drawer")', 1000);
  await js('[...document.querySelectorAll(".queue-head button")].find(button => button.textContent.trim() === "Clear")?.click()');
  await waitUntil('document.querySelectorAll(".queue-row").length === 0', 2000);
  await closeQueue();

  // Start a real queue through the UI, then save its exact current order.
  const firstTitle = await js('document.querySelectorAll(".track strong")[0]?.textContent');
  await js('document.querySelectorAll(".track-play")[0]?.click()');
  await js('if (!document.querySelector(".queue-drawer")) document.querySelector("[aria-label=Queue]")?.click()');
  const queued = !!firstTitle && await waitUntil(`document.querySelectorAll('.queue-row')[0]?.querySelector('.q-title')?.textContent === ${JSON.stringify(firstTitle)}`, 3000) < Infinity;
  check('curation: track play creates a queue', queued);
  if (!queued) return closeQueue();
  const saveState = await js(`(() => {
    const save = document.querySelector('.queue-drawer .save-queue-playlist');
    return save ? { disabled: save.disabled, queue: [...document.querySelectorAll('.queue-row .q-title')].map(row => row.textContent) } : null;
  })()`);
  console.log(`  curation save control: ${JSON.stringify(saveState)}`);
  const drawerOpen = !!saveState && !saveState.disabled;
  check('curation: queue can be saved', drawerOpen);
  if (!drawerOpen) return closeQueue();
  const queuedIds = await js('[...document.querySelectorAll(".queue-row")].map(row => row.querySelector(".q-title")?.textContent)');
  await js("document.querySelector('.save-queue-playlist')?.click()");
  await waitUntil('document.querySelector(".modal input[name=value]")', 1000);
  await js(`(() => { const input = document.querySelector('.modal input[name=value]'); if (input) { input.value = 'Curation queue'; input.form.requestSubmit(); } })()`);
  const saved = await waitUntil('document.querySelector(".hero-title")?.textContent === "Curation queue"', 3000) < Infinity;
  check('curation: saving queue opens editable playlist', saved);
  check('curation: saved playlist is not covered by the queue drawer', saved && !(await js('document.querySelector(".queue-drawer")')));
  const savedDetail = await api('/api/playlists');
  const playlist = savedDetail?.find((item) => item.name === 'Curation queue');
  const savedTracks = playlist ? await api(`/api/playlists/${playlist.id}`) : null;
  check('curation: saved playlist preserves queue order', !!savedTracks && JSON.stringify(savedTracks.tracks.map((track) => track.title)) === JSON.stringify(queuedIds));
  if (!playlist) return closeQueue();

  // Playlist detail supports the three membership edits listeners need while curating.
  await js("document.querySelector('.rename-playlist')?.click()");
  await waitUntil('document.querySelector(".modal input[name=value]")', 1000);
  await js(`(() => { const input = document.querySelector('.modal input[name=value]'); if (input) { input.value = 'Renamed curation queue'; input.form.requestSubmit(); } })()`);
  check('curation: playlist can be renamed', await waitUntil('document.querySelector(".hero-title")?.textContent === "Renamed curation queue"', 2000) < Infinity);
  const beforeReorder = await js('[...document.querySelectorAll(".track")].map(row => row.querySelector("strong")?.textContent)');
  await js(`(() => {
    const original = window.fetch;
    let failOnce = true;
    window.__restoreCurationFetch = () => { window.fetch = original; };
    window.fetch = (input, init) => {
      if (failOnce && init?.method === 'PATCH' && String(input).includes('/api/playlists/')) {
        failOnce = false;
        return Promise.resolve(new Response(JSON.stringify({error:{message:'Playlist update unavailable'}}), {status:503, headers:{'content-type':'application/json'}}));
      }
      return original(input, init);
    };
  })()`);
  await js('document.querySelector(".playlist-move-down")?.click()');
  check('curation: failed playlist edit remains visible and retryable', await waitUntil('document.querySelector("[role=alert]")?.textContent.includes("Playlist update unavailable") && !document.querySelector(".playlist-move-down")?.disabled', 2000) < Infinity);
  await js('document.querySelector(".playlist-edit-retry")?.click()');
  check('curation: playlist track can move down', beforeReorder.length < 2 || await waitUntil(`document.querySelectorAll('.track strong')[0]?.textContent === ${JSON.stringify(beforeReorder[1])}`, 2000) < Infinity);
  await js('window.__restoreCurationFetch?.()');
  const beforeRemove = await js('document.querySelectorAll(".track").length');
  await js('document.querySelector(".playlist-remove")?.click()');
  check('curation: playlist membership can be removed', await waitUntil(`document.querySelectorAll('.track').length === ${Math.max(0, beforeRemove - 1)}`, 2000) < Infinity);

  // Re-enter Tracks so its playlist picker loads the newly saved playlist.
  const targetPlaylist = await api('/api/playlists', {
    method: 'POST', headers: {'content-type': 'application/json'},
    body: JSON.stringify({name: 'Curation target'}),
  });
  await clickText('.seg', 'Tracks');
  await waitUntil('document.querySelector(".track-add-playlist")', 2000);
  await js('if (!document.querySelector(".queue-drawer")) document.querySelector("[aria-label=Queue]")?.click()');
  await waitUntil('document.querySelector(".queue-drawer")', 1000);
  const queueBeforeAdd = await js('document.querySelectorAll(".queue-row").length');
  await js('document.querySelectorAll(".track-add-queue")[0]?.click()');
  check('curation: browse track can be added to queue', await waitUntil(`document.querySelectorAll('.queue-row').length === ${queueBeforeAdd + 1}`, 2000) < Infinity);
  const nextTitle = await js('document.querySelectorAll(".track strong")[1]?.textContent');
  await js('document.querySelectorAll(".track-play-next")[1]?.click()');
  check('curation: browse track can play next', !!nextTitle && await waitUntil(`document.querySelectorAll('.queue-row')[1]?.querySelector('.q-title')?.textContent === ${JSON.stringify(nextTitle)}`, 2000) < Infinity);
  const selectedTitle = await js('document.querySelector(".track strong")?.textContent');
  await js(`(() => { const select = document.querySelectorAll('.track-add-playlist')[0]; select.value = ${JSON.stringify(targetPlaylist?.id)}; select.dispatchEvent(new Event('change', {bubbles:true})); })()`);
  await new Promise((resolve) => setTimeout(resolve, 250));
  const updated = targetPlaylist && await api(`/api/playlists/${targetPlaylist.id}`);
  check('curation: browse track can be added to a saved playlist', !!updated && updated.tracks.some((track) => track.title === selectedTitle));
  await closeQueue();
}
