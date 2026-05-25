const state = {
  albums: [],
  tracks: [],
  visibleTracks: [],
  currentIndex: -1,
  searchController: null,
};

const els = {
  summary: document.querySelector("#summary"),
  search: document.querySelector("#search"),
  albums: document.querySelector("#albums"),
  trackList: document.querySelector("#track-list"),
  viewTitle: document.querySelector("#view-title"),
  refresh: document.querySelector("#refresh"),
  audio: document.querySelector("#audio"),
  nowArt: document.querySelector("#now-art"),
  nowTitle: document.querySelector("#now-title"),
  nowSubtitle: document.querySelector("#now-subtitle"),
  prev: document.querySelector("#prev"),
  next: document.querySelector("#next"),
  playPause: document.querySelector("#play-pause"),
};

async function api(path) {
  const response = await fetch(path);
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.json();
}

async function apiPost(path) {
  const response = await fetch(path, { method: "POST" });
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.json();
}

async function searchApi(query, signal) {
  const response = await fetch(`/api/search?q=${encodeURIComponent(query)}`, { signal });
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.json();
}

async function loadLibrary() {
  try {
    const [summary, albums, tracks] = await Promise.all([
      api("/api/library/summary"),
      api("/api/albums"),
      api("/api/tracks"),
    ]);

    state.albums = albums;
    state.tracks = tracks;
    state.visibleTracks = tracks;
    els.summary.textContent = `${summary.track_count} tracks, ${summary.album_count} albums`;
    els.viewTitle.textContent = "Tracks";
    renderAlbums(albums);
    renderTracks(tracks);
  } catch (error) {
    els.trackList.innerHTML = `<p class="error">Failed to load library: ${escapeHtml(error.message)}</p>`;
  }
}

async function rescanLibrary() {
  const label = els.refresh.textContent;
  els.refresh.disabled = true;
  els.refresh.textContent = "Scanning";

  try {
    const result = await apiPost("/api/library/rescan");
    await loadLibrary();
    const status = result.updated ? "" : " (unchanged)";
    els.summary.textContent = `${result.summary.track_count} tracks, ${result.summary.album_count} albums${status}`;
  } catch (error) {
    els.trackList.innerHTML = `<p class="error">Rescan failed: ${escapeHtml(error.message)}</p>`;
  } finally {
    els.refresh.disabled = false;
    els.refresh.textContent = label;
  }
}

function renderAlbums(albums) {
  els.albums.innerHTML = "";

  for (const album of albums) {
    const button = document.createElement("button");
    button.className = "album";
    button.type = "button";
    button.innerHTML = `
      ${album.artwork_url ? `<img src="${album.artwork_url}" alt="">` : `<span class="album-placeholder"></span>`}
      <span>
        <strong>${escapeHtml(album.title)}</strong>
        <span>${escapeHtml(album.artist_name)}${album.year ? ` · ${album.year}` : ""}</span>
      </span>
    `;
    button.addEventListener("click", () => {
      const tracks = state.tracks.filter((track) => track.album_id === album.id);
      state.visibleTracks = tracks;
      els.viewTitle.textContent = album.title;
      renderTracks(tracks);
    });
    els.albums.append(button);
  }
}

function renderTracks(tracks) {
  els.trackList.innerHTML = "";

  if (tracks.length === 0) {
    els.trackList.innerHTML = `<p class="empty">No tracks found.</p>`;
    return;
  }

  for (const [index, track] of tracks.entries()) {
    const button = document.createElement("button");
    button.className = "track";
    button.type = "button";
    button.dataset.trackId = track.id;
    button.innerHTML = `
      <span>
        <strong>${escapeHtml(track.title)}</strong>
        <span>${escapeHtml(track.artist_name)}</span>
      </span>
      <span>${escapeHtml(track.album_title)}</span>
      <small>${track.extension.toUpperCase()}</small>
    `;
    button.addEventListener("click", () => playTrack(index));
    els.trackList.append(button);
  }

  markActiveTrack();
}

function playTrack(index) {
  const track = state.visibleTracks[index];
  if (!track) {
    return;
  }

  state.currentIndex = index;
  els.audio.src = track.stream_url;
  els.audio.play().catch(() => {});
  updateNowPlaying(track);
  markActiveTrack();
}

function updateNowPlaying(track) {
  const album = state.albums.find((item) => item.id === track.album_id);
  els.nowTitle.textContent = track.title;
  els.nowSubtitle.textContent = `${track.artist_name} - ${track.album_title}`;
  els.nowArt.src = album?.artwork_url || "";
  els.nowArt.hidden = !album?.artwork_url;

  if ("mediaSession" in navigator) {
    navigator.mediaSession.metadata = new MediaMetadata({
      title: track.title,
      artist: track.artist_name,
      album: track.album_title,
      artwork: album?.artwork_url ? [{ src: album.artwork_url }] : [],
    });
  }
}

function markActiveTrack() {
  for (const row of els.trackList.querySelectorAll(".track")) {
    row.classList.toggle(
      "active",
      row.dataset.trackId === state.visibleTracks[state.currentIndex]?.id,
    );
  }
}

async function search() {
  const query = els.search.value.trim();
  if (!query) {
    state.searchController?.abort();
    state.searchController = null;
    state.visibleTracks = state.tracks;
    els.viewTitle.textContent = "Tracks";
    renderAlbums(state.albums);
    renderTracks(state.tracks);
    return;
  }

  state.searchController?.abort();
  const controller = new AbortController();
  state.searchController = controller;

  try {
    const results = await searchApi(query, controller.signal);
    if (state.searchController !== controller) {
      return;
    }

    state.visibleTracks = results.tracks;
    els.viewTitle.textContent = `Search: ${query} (${results.tracks.length} tracks, ${results.albums.length} albums, ${results.artists.length} artists)`;
    renderAlbums(searchAlbums(results));
    renderTracks(results.tracks);
  } catch (error) {
    if (error.name === "AbortError") {
      return;
    }
    els.trackList.innerHTML = `<p class="error">Search failed: ${escapeHtml(error.message)}</p>`;
  }
}

function searchAlbums(results) {
  const albumIds = new Set(results.albums.map((album) => album.id));
  for (const track of results.tracks) {
    albumIds.add(track.album_id);
  }

  return state.albums.filter((album) => albumIds.has(album.id));
}

function playNext(offset) {
  if (state.visibleTracks.length === 0) {
    return;
  }

  const nextIndex = state.currentIndex < 0
    ? 0
    : (state.currentIndex + offset + state.visibleTracks.length) % state.visibleTracks.length;
  playTrack(nextIndex);
}

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

els.search.addEventListener("input", debounce(search, 180));
els.search.addEventListener("search", search);
els.search.addEventListener("change", search);
els.refresh.addEventListener("click", rescanLibrary);
els.prev.addEventListener("click", () => playNext(-1));
els.next.addEventListener("click", () => playNext(1));
els.audio.addEventListener("ended", () => playNext(1));
els.audio.addEventListener("play", () => {
  els.playPause.textContent = "Pause";
});
els.audio.addEventListener("pause", () => {
  els.playPause.textContent = "Play";
});
els.playPause.addEventListener("click", () => {
  if (els.audio.paused) {
    if (!els.audio.src) {
      playTrack(0);
    } else {
      els.audio.play().catch(() => {});
    }
  } else {
    els.audio.pause();
  }
});

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

loadLibrary();

function debounce(fn, delay) {
  let timer = 0;
  return (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), delay);
  };
}
