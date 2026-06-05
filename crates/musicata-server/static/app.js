const state = {
  albums: [],
  visibleAlbums: [],
  browse: { genres: [], years: [], composers: [] },
  browseFilter: { genre: "", year: "", composer: "" },
  tracks: [],
  visibleTracks: [],
  view: "library",
  albumSort: "",
  artistSort: "",
  navStack: [],
  librarySignature: null,
  lastNow: {},
  favoriteTrackIds: new Set(),
  playlists: [],
  currentPlaylistId: null,
  smartPlaylists: [],
  currentSmartId: null,
  addMenu: null,
  radio: [],
  trackStream: null,
  albumStream: null,
  activePlayerId: null,
  zones: [],
  activeStatus: "stopped",
  activeNowTrackId: null,
  activeState: null,
  activeElapsed: 0,
  activeDuration: 0,
  seekDragging: false,
  queueOpen: false,
  playerStatus: {},
  playerMenuOpen: false,
  searchController: null,
  metadataTrackId: null,
  metadataTrackObj: null,
  metadataReview: null,
  metadataError: "",
  metadataTrackCandidates: null,
  metadataAlbumCandidates: null,
  metadataCandidateLoading: "",
  metadataArtworkReview: null,
  metadataArtworkError: "",
  metadataArtworkLoading: false,
  metadataCoverArtCandidates: null,
  metadataCoverArtError: "",
  metadataCoverArtLoading: false,
};

const els = {
  toast: document.querySelector("#toast"),
  summary: document.querySelector("#summary"),
  search: document.querySelector("#search"),
  browseGenre: document.querySelector("#browse-genre"),
  browseYear: document.querySelector("#browse-year"),
  browseComposer: document.querySelector("#browse-composer"),
  browseClear: document.querySelector("#browse-clear"),
  playlists: document.querySelector("#playlists"),
  smartPlaylists: document.querySelector("#smart-playlists"),
  newPlaylist: document.querySelector("#new-playlist"),
  newPlaylistForm: document.querySelector("#new-playlist-form"),
  newPlaylistName: document.querySelector("#new-playlist-name"),
  newPlaylistError: document.querySelector("#new-playlist-error"),
  radio: document.querySelector("#radio"),
  newRadio: document.querySelector("#new-radio"),
  radioDirectory: document.querySelector("#radio-directory"),
  radioDirClose: document.querySelector("#radio-dir-close"),
  radioSearch: document.querySelector("#radio-search"),
  radioResults: document.querySelector("#radio-results"),
  radioAddUrl: document.querySelector("#radio-add-url"),
  trackList: document.querySelector("#track-list"),
  browseGrid: document.querySelector("#browse-grid"),
  detailHero: document.querySelector("#detail-hero"),
  navLinks: Array.from(document.querySelectorAll(".library-nav .nav-link")),
  segButtons: Array.from(document.querySelectorAll("#segmented .seg")),
  sortControl: document.querySelector(".sort-control"),
  sortSelect: document.querySelector("#sort-select"),
  backBtn: document.querySelector("#back-btn"),
  viewTitle: document.querySelector("#view-title"),
  audio: document.querySelector("#audio"),
  nowTitle: document.querySelector("#now-title"),
  nowSubtitle: document.querySelector("#now-subtitle"),
  prev: document.querySelector("#prev"),
  next: document.querySelector("#next"),
  playPause: document.querySelector("#play-pause"),
  switchBtn: document.querySelector("#player-switch-btn"),
  switchName: document.querySelector("#switch-name"),
  switchSignal: document.querySelector("#switch-signal"),
  playerMenu: document.querySelector("#player-menu"),
  playerMenuList: document.querySelector("#player-menu-list"),
  playerMenuConfig: document.querySelector("#player-menu-config"),
  transport: document.querySelector(".transport"),
  nowArt: document.querySelector("#now-art"),
  nowArtFallback: document.querySelector("#now-art-fallback"),
  seek: document.querySelector("#seek"),
  elapsed: document.querySelector("#elapsed"),
  duration: document.querySelector("#duration"),
  shuffle: document.querySelector("#shuffle"),
  repeat: document.querySelector("#repeat"),
  footerVolume: document.querySelector("#footer-volume"),
  queueToggle: document.querySelector("#queue-toggle"),
  queueCount: document.querySelector("#queue-count"),
  queueDrawer: document.querySelector("#queue-drawer"),
  queueList: document.querySelector("#queue-list"),
  queueClear: document.querySelector("#queue-clear"),
  queueClose: document.querySelector("#queue-close"),
  metadataPanel: document.querySelector("#metadata-panel"),
  metadataTitle: document.querySelector("#metadata-title"),
  metadataBody: document.querySelector("#metadata-body"),
  metadataClose: document.querySelector("#metadata-close"),
  menuToggle: document.querySelector("#menu-toggle"),
  mobileSettings: document.querySelector("#mobile-settings"),
  scrim: document.querySelector("#scrim"),
  transportNow: document.querySelector(".transport-now"),
  miniPlay: document.querySelector("#mini-play"),
  npExpand: document.querySelector("#np-expand"),
  npCollapse: document.querySelector("#np-collapse"),
};

// Players, music sources and background activity live on the dedicated /admin page.
function openAdmin() {
  window.location.href = "/admin";
}

// ---- Mobile chrome: sidebar drawer + now-playing sheet ----
function setDrawer(open) {
  document.body.classList.toggle("nav-open", open);
  els.scrim.hidden = !open;
  els.menuToggle.setAttribute("aria-expanded", String(open));
}
function setNowPlayingSheet(open) {
  document.body.classList.toggle("np-open", open);
}
function closeDrawerOnMobile() {
  if (document.body.classList.contains("nav-open")) setDrawer(false);
}
els.menuToggle.addEventListener("click", () =>
  setDrawer(!document.body.classList.contains("nav-open")),
);
els.scrim.addEventListener("click", () => setDrawer(false));
els.mobileSettings.addEventListener("click", openAdmin);
els.npExpand.addEventListener("click", () => setNowPlayingSheet(true));
els.npCollapse.addEventListener("click", () => setNowPlayingSheet(false));
// Tapping the now-playing strip (but not its buttons) expands the sheet on mobile.
els.transportNow.addEventListener("click", (event) => {
  if (event.target.closest(".mini-controls")) return;
  if (window.matchMedia("(max-width: 820px)").matches && !document.body.classList.contains("np-open")) {
    setNowPlayingSheet(true);
  }
});
els.miniPlay.addEventListener("click", () => els.playPause.click());

document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  if (!els.radioDirectory.hidden) closeRadioDirectory();
  else if (document.body.classList.contains("np-open")) setNowPlayingSheet(false);
  else if (document.body.classList.contains("nav-open")) setDrawer(false);
});

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

async function apiPatch(path, body) {
  const response = await fetch(path, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.json();
}

// How many rows we pull per scroll page. Lists load one page up front and fetch the
// next as a bottom sentinel scrolls into view (infinite scroll, no explicit pages).
const PAGE_SIZE = 100;

async function searchApi(query, signal, offset = 0, limit = PAGE_SIZE) {
  const params = new URLSearchParams({ q: query, limit: String(limit), offset: String(offset) });
  const response = await fetch(`/api/search?${params}`, { signal });
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.json();
}

// One page of /api/tracks (a pagination envelope; callers want the items array).
async function tracksApi(filter = {}, offset = 0, limit = PAGE_SIZE) {
  const params = new URLSearchParams({ limit: String(limit), offset: String(offset) });
  if (filter.genre) {
    params.set("genre", filter.genre);
  }
  if (filter.year) {
    params.set("year", filter.year);
  }
  if (filter.composer) {
    params.set("composer", filter.composer);
  }
  if (filter.folder) {
    params.set("folder", filter.folder);
  }
  const page = await api(`/api/tracks?${params}`);
  return page.items;
}

function browseApi() {
  return api("/api/browse");
}

// One page of /api/albums. With a browse filter set, the server returns only albums
// whose tracks match it, so the album grid narrows the same way the track list does.
async function albumsApi(filter = {}, offset = 0, limit = ALBUM_PAGE, sort = "") {
  const params = new URLSearchParams({ limit: String(limit), offset: String(offset) });
  if (sort) {
    params.set("sort", sort);
  }
  if (filter.genre) {
    params.set("genre", filter.genre);
  }
  if (filter.year) {
    params.set("year", filter.year);
  }
  if (filter.composer) {
    params.set("composer", filter.composer);
  }
  if (filter.folder) {
    params.set("folder", filter.folder);
  }
  const page = await api(`/api/albums?${params}`);
  return page.items;
}

// ---- Infinite scroll ----
// Fill `container` one page at a time, pulling the next page when a bottom sentinel
// scrolls into view — no explicit pages, just keep scrolling. `getPage(offset)`
// returns (a promise of) the next items; a page shorter than `pageSize` ends the
// stream. `appendBatch(items, offset)` builds and inserts the DOM for one page. The
// returned controller's `destroy()` detaches the observer — call it before starting a
// new stream in the same container. The container is its own scroll root.
function infiniteScroll(container, getPage, appendBatch, { pageSize = PAGE_SIZE } = {}) {
  let offset = 0;
  let done = false;
  let loading = false;
  let alive = true;

  const root = scrollParent(container); // the actual clipping scroller (null = viewport)
  const sentinel = document.createElement("div");
  sentinel.className = "scroll-sentinel";
  sentinel.setAttribute("aria-hidden", "true");

  async function loadNext() {
    if (!alive || done || loading) return;
    loading = true;
    try {
      const items = (await getPage(offset)) || [];
      if (!alive) return;
      appendBatch(items, offset);
      container.append(sentinel); // keep the sentinel last, after the appended rows
      offset += items.length;
      if (items.length < pageSize) {
        done = true;
        observer.disconnect();
        sentinel.remove();
        return;
      }
    } catch (error) {
      done = true; // stop hammering a failing endpoint
      observer.disconnect();
      console.error("infinite scroll page failed:", error);
      return;
    } finally {
      loading = false;
    }
    // Sentinel still on screen (tall viewport or short page)? Keep filling.
    if (alive && !done && onScreen(sentinel, root)) loadNext();
  }

  const observer = new IntersectionObserver(
    (entries) => { if (entries.some((e) => e.isIntersecting)) loadNext(); },
    { root, rootMargin: "600px" },
  );
  container.append(sentinel);
  observer.observe(sentinel);
  loadNext(); // first page

  return { destroy() { alive = false; observer.disconnect(); sentinel.remove(); } };
}

// Nearest scrollable ancestor — the element whose viewport the sentinel must enter.
function scrollParent(el) {
  let node = el.parentElement;
  while (node && node !== document.body) {
    const overflowY = getComputedStyle(node).overflowY;
    if (overflowY === "auto" || overflowY === "scroll") return node;
    node = node.parentElement;
  }
  return null;
}

function onScreen(el, root) {
  const r = el.getBoundingClientRect();
  const top = root ? root.getBoundingClientRect().top : 0;
  const bottom = root ? root.getBoundingClientRect().bottom : window.innerHeight;
  return r.top <= bottom && r.bottom >= top;
}

async function loadLibrary() {
  try {
    // Tracks and albums are paged in by their views on demand (a big library is megabytes),
    // so startup only needs the summary + the browse facets.
    const [summary, browse] = await Promise.all([api("/api/library/summary"), browseApi()]);

    state.browse = browse;
    els.summary.textContent = `${summary.track_count} tracks, ${summary.album_count} albums`;
    state.librarySignature = `${summary.track_count}:${summary.album_count}`;
    renderBrowseFilters();
    await refreshCurrentView();
    renderMetadataPanel();
  } catch (error) {
    els.trackList.innerHTML = `<p class="error">Failed to load library: ${escapeHtml(error.message)}</p>`;
  }
}

// Re-render whatever view is active (after a library sync). Detail/playlist views are
// left untouched so a background refresh doesn't yank the user out of them.
function refreshCurrentView() {
  switch (state.view) {
    case "albums":
      return openAlbumsView();
    case "artists":
      return openArtistsView();
    case "favorites":
      return openFavoritesView();
    case "recent":
    case "most":
      return loadHistoryView(state.view);
    case "album-detail":
    case "artist-detail":
    case "playlist":
      return Promise.resolve();
    default:
      if (els.search.value.trim()) return search();
      return applyBrowseFilter({ clearSearch: false });
  }
}

// Cheap poll: re-read the library only when its track/album counts changed (the
// server rescans the filesystem on its own). This replaces the manual refresh
// button — new or removed tracks surface without any user action.
async function syncLibrary() {
  try {
    const summary = await api("/api/library/summary");
    const signature = `${summary.track_count}:${summary.album_count}`;
    if (signature !== state.librarySignature) {
      await loadLibrary();
    }
  } catch {
    /* transient; the next tick retries */
  }
}

// Render the album sidebar incrementally: album metadata is fully in memory (cheap),
// but cards (and their artwork requests) are appended a chunk at a time as you scroll,
// so a large library doesn't build thousands of nodes or fire an image storm up front.
const ALBUM_PAGE = 80;

// Stream cards into the main browse grid from a page source (server-paged or a
// client-sliced array), appending the next chunk as the grid scrolls.
function streamBrowseGrid(getPage, buildCard) {
  state.albumStream?.destroy();
  els.browseGrid.innerHTML = "";
  state.albumStream = infiniteScroll(
    els.browseGrid,
    getPage,
    (batch) => { for (const item of batch) els.browseGrid.append(buildCard(item)); },
    { pageSize: ALBUM_PAGE },
  );
}

// Show exactly the content panels a view needs; clear the others (and the grid stream).
function showPanels({ hero = false, grid = false, tracks = false }) {
  els.detailHero.hidden = !hero;
  if (!hero) els.detailHero.innerHTML = "";
  els.browseGrid.hidden = !grid;
  if (!grid) {
    state.albumStream?.destroy();
    state.albumStream = null;
    els.browseGrid.innerHTML = "";
  }
  els.trackList.hidden = !tracks;
}

// --- Browse back-stack: root views (segmented tabs / sidebar nav) reset it; detail
// views push a "render the previous screen" closure. The browser Back button drives it
// through popstate, so in-app Back and the browser button behave the same. ---
function resetNav() {
  state.navStack = [];
  els.backBtn.hidden = true;
}
function pushNav(renderPrevious) {
  state.navStack.push(renderPrevious);
  els.backBtn.hidden = false;
  history.pushState({ depth: state.navStack.length }, "");
}
function popNav() {
  const renderPrevious = state.navStack.pop();
  els.backBtn.hidden = state.navStack.length === 0;
  if (renderPrevious) renderPrevious();
}

// Which segmented tab is highlighted for a given view.
const SEG_FOR_VIEW = {
  library: "library",
  albums: "albums",
  "album-detail": "albums",
  artists: "artists",
  "artist-detail": "artists",
};

// Sort options per browse view: [value, label]. "" = the server's default order.
const SORTS = {
  albums: [["", "Artist"], ["title", "Title (A–Z)"], ["year", "Year"]],
  artists: [["", "Name (A–Z)"], ["tracks", "Most tracks"], ["albums", "Most albums"]],
};
function renderSort(view) {
  const options = SORTS[view];
  els.sortControl.hidden = !options;
  if (!options) return;
  const current = view === "albums" ? state.albumSort : state.artistSort;
  els.sortSelect.innerHTML = "";
  for (const [value, label] of options) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = label;
    if (value === current) option.selected = true;
    els.sortSelect.append(option);
  }
}

async function artistsApi(sort = "", offset = 0, limit = ALBUM_PAGE) {
  const params = new URLSearchParams({ limit: String(limit), offset: String(offset) });
  if (sort) params.set("sort", sort);
  const page = await api(`/api/artists?${params}`);
  return page.items;
}

// The Albums browse view: a full-width card grid, honoring the active browse filter and
// sort, paged from the server.
async function openAlbumsView() {
  markNavActive("albums");
  closeDrawerOnMobile();
  showPanels({ grid: true });
  renderSort("albums");
  els.viewTitle.textContent = hasBrowseFilter() ? browseTitle(state.browseFilter) : "Albums";
  streamBrowseGrid(
    (offset) => albumsApi(state.browseFilter, offset, ALBUM_PAGE, state.albumSort),
    buildAlbumCard,
  );
}

// The Artists browse view: a card grid paged from the server.
async function openArtistsView() {
  markNavActive("artists");
  closeDrawerOnMobile();
  showPanels({ grid: true });
  renderSort("artists");
  els.viewTitle.textContent = "Artists";
  streamBrowseGrid((offset) => artistsApi(state.artistSort, offset), buildArtistCard);
}

// Request a downscaled cover variant from the server (a `?size=` thumbnail). Respects an
// existing query string (acquired covers already carry `?asset=`).
function sizedArtwork(url, size) {
  if (!url) return url;
  return `${url}${url.includes("?") ? "&" : "?"}size=${size}`;
}

// The monogram shown for a coverless album: first letter of the title (or artist).
function albumInitial(album) {
  const source = (album.title || album.artist_name || "?").trim();
  return (source.charAt(0) || "?").toUpperCase();
}

// The inside of an artist avatar: their acquired image when present, else a monogram.
// The <img> falls back to the monogram if it fails to load (e.g. cleared cache).
function artistAvatarInner(artist, size) {
  const initial = (artist.name || "?").trim().charAt(0).toUpperCase() || "?";
  const mono = escapeHtml(initial);
  if (!artist.artwork_url) return mono;
  return `<img src="${sizedArtwork(artist.artwork_url, size)}" alt="" loading="lazy"
    onerror="this.replaceWith(document.createTextNode('${mono}'))">`;
}

// A cover-forward album card for the browse grid. Click → album detail; the play overlay
// plays the album; `back` is the closure that re-renders where this card was opened from.
function buildAlbumCard(album, back) {
  const card = document.createElement("div");
  card.className = "card album-card";
  card.dataset.albumId = album.id;
  card.setAttribute("role", "button");
  card.tabIndex = 0;
  const cover = album.artwork_url
    ? `<img src="${sizedArtwork(album.artwork_url, 300)}" alt="" loading="lazy">`
    : `<span class="album-placeholder">${escapeHtml(albumInitial(album))}</span>`;
  card.innerHTML = `
    <div class="card-cover">
      ${cover}
      <button class="card-play" type="button" title="Play album" aria-label="Play album">▶</button>
    </div>
    <div class="card-text">
      <strong>${escapeHtml(album.title)}</strong>
      <span>${escapeHtml(album.artist_name)}${album.year ? ` · ${album.year}` : ""}</span>
    </div>
  `;
  const open = () => openAlbum(album, back);
  card.addEventListener("click", open);
  card.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(); }
  });
  card.querySelector(".card-play").addEventListener("click", (event) => {
    event.stopPropagation();
    playAlbum(album);
  });
  return card;
}

// An artist card (monogram + name + counts). Click → artist detail.
function buildArtistCard(artist, back) {
  const card = document.createElement("div");
  card.className = "card artist-card";
  card.dataset.artistId = artist.id;
  card.setAttribute("role", "button");
  card.tabIndex = 0;
  card.innerHTML = `
    <div class="artist-avatar" aria-hidden="true">${artistAvatarInner(artist, 300)}</div>
    <div class="card-text">
      <strong>${escapeHtml(artist.name)}</strong>
      <span>${countLabel(artist.album_count, "album")} · ${countLabel(artist.track_count, "track")}</span>
    </div>
  `;
  const open = () => openArtist(artist, back);
  card.addEventListener("click", open);
  card.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(); }
  });
  return card;
}

function countLabel(n, noun) {
  return `${n} ${n === 1 ? noun : `${noun}s`}`;
}

// A whole-track-list total duration, e.g. "42 min" or "1 h 5 min".
function formatDuration(totalSeconds) {
  const minutes = Math.round(totalSeconds / 60);
  if (minutes < 60) return `${minutes} min`;
  return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

function shuffleIds(ids) {
  const out = ids.slice();
  for (let i = out.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [out[i], out[j]] = [out[j], out[i]];
  }
  return out;
}

// Play a whole album now (from its first track).
async function playAlbum(album) {
  try {
    const detail = await api(`/api/albums/${encodeURIComponent(album.id)}`);
    const tracks = detail.tracks || [];
    playTrackIds(tracks.map((track) => track.id), 0, tracks[0]?.stream_url || null);
  } catch (error) {
    showToast(error.message || "Couldn’t play the album.");
  }
}

// Wire a hero's Play / Shuffle buttons to a track list (with within-gesture autoplay).
function wireHeroPlayback(tracks) {
  const ids = tracks.map((track) => track.id);
  els.detailHero
    .querySelector(".hero-play")
    .addEventListener("click", () => playTrackIds(ids, 0, tracks[0]?.stream_url || null));
  els.detailHero.querySelector(".hero-shuffle").addEventListener("click", () => {
    const order = shuffleIds(tracks);
    playTrackIds(order.map((track) => track.id), 0, order[0]?.stream_url || null);
  });
}

// Album detail: a hero header (cover, title, artist link, meta, Play/Shuffle) + tracks.
async function openAlbum(album, back) {
  if (back) pushNav(back);
  markNavActive("album-detail");
  closeDrawerOnMobile();
  showPanels({ hero: true, tracks: true });
  renderSort(null);
  els.viewTitle.textContent = album.title;
  els.detailHero.innerHTML = `<p class="empty">Loading…</p>`;
  try {
    const detail = await api(`/api/albums/${encodeURIComponent(album.id)}`);
    renderAlbumHero(detail);
    renderTracks(detail.tracks);
  } catch (error) {
    els.detailHero.innerHTML = "";
    els.trackList.innerHTML = `<p class="error">Failed to load album: ${escapeHtml(error.message)}</p>`;
  }
}

// Artist detail: a hero header + the artist's albums as a card grid.
async function openArtist(artist, back) {
  if (back) pushNav(back);
  markNavActive("artist-detail");
  closeDrawerOnMobile();
  showPanels({ hero: true, grid: true });
  renderSort(null);
  els.viewTitle.textContent = artist.name;
  els.detailHero.innerHTML = `<p class="empty">Loading…</p>`;
  try {
    const detail = await api(`/api/artists/${encodeURIComponent(artist.id)}`);
    renderArtistHero(detail);
    const albums = detail.albums || [];
    const backToArtist = () => openArtist(detail.artist);
    streamBrowseGrid(
      (offset) => albums.slice(offset, offset + ALBUM_PAGE),
      (album) => buildAlbumCard(album, backToArtist),
    );
  } catch (error) {
    els.detailHero.innerHTML = "";
    els.browseGrid.innerHTML = `<p class="error">Failed to load artist: ${escapeHtml(error.message)}</p>`;
  }
}

function renderAlbumHero(detail) {
  const album = detail.album;
  const tracks = detail.tracks || [];
  const total = tracks.reduce((sum, track) => sum + (track.duration_seconds || 0), 0);
  const meta = [
    album.year ? String(album.year) : null,
    countLabel(tracks.length, "track"),
    total ? formatDuration(total) : null,
  ].filter(Boolean).join(" · ");
  els.detailHero.innerHTML = `
    <div class="hero-cover">${album.artwork_url
      ? `<img src="${sizedArtwork(album.artwork_url, 600)}" alt="">`
      : `<span class="album-placeholder">${escapeHtml(albumInitial(album))}</span>`}</div>
    <div class="hero-info">
      <p class="eyebrow">Album</p>
      <h2 class="hero-title">${escapeHtml(album.title)}</h2>
      <p class="hero-sub"><button class="link-artist" type="button">${escapeHtml(album.artist_name)}</button><span class="hero-meta"> · ${escapeHtml(meta)}</span></p>
      <div class="hero-actions">
        <button class="primary-button hero-play" type="button">▶ Play</button>
        <button class="ghost-button hero-shuffle" type="button">⤨ Shuffle</button>
      </div>
    </div>
  `;
  wireHeroPlayback(tracks);
  const artistLink = els.detailHero.querySelector(".link-artist");
  if (detail.artist) {
    artistLink.addEventListener("click", () => openArtist(detail.artist, () => openAlbum(album)));
  } else {
    artistLink.disabled = true;
  }
}

function renderArtistHero(detail) {
  const artist = detail.artist;
  const tracks = detail.tracks || [];
  const meta = `${countLabel(artist.album_count, "album")} · ${countLabel(artist.track_count, "track")}`;
  els.detailHero.innerHTML = `
    <div class="hero-cover artist"><span class="artist-avatar large" aria-hidden="true">${artistAvatarInner(artist, 600)}</span></div>
    <div class="hero-info">
      <p class="eyebrow">Artist</p>
      <h2 class="hero-title">${escapeHtml(artist.name)}</h2>
      <p class="hero-sub">${escapeHtml(meta)}</p>
      <div class="hero-actions">
        <button class="primary-button hero-play" type="button">▶ Play</button>
        <button class="ghost-button hero-shuffle" type="button">⤨ Shuffle</button>
      </div>
    </div>
  `;
  wireHeroPlayback(tracks);
}

function renderBrowseFilters() {
  renderBrowseSelect(els.browseGenre, "All genres", state.browse.genres, state.browseFilter.genre);
  renderBrowseSelect(els.browseYear, "All years", state.browse.years, state.browseFilter.year);
  renderBrowseSelect(els.browseComposer, "All composers", state.browse.composers, state.browseFilter.composer);
  els.browseClear.disabled = !hasBrowseFilter();
}

function renderBrowseSelect(select, emptyLabel, facets, selectedValue) {
  select.innerHTML = "";
  const empty = document.createElement("option");
  empty.value = "";
  empty.textContent = emptyLabel;
  select.append(empty);

  for (const facet of facets) {
    const option = document.createElement("option");
    option.value = String(facet.value);
    option.textContent = `${facet.value} (${facet.track_count})`;
    select.append(option);
  }

  select.value = selectedValue || "";
}

async function applyBrowseFilter(options = {}) {
  const clearSearch = options.clearSearch !== false;
  state.browseFilter = currentBrowseFilter();
  renderBrowseFilters();

  if (clearSearch) {
    state.searchController?.abort();
    state.searchController = null;
    els.search.value = "";
  }

  // A facet change while browsing Albums re-filters the album grid; otherwise it filters
  // the track list (the Tracks view).
  if (SEG_FOR_VIEW[state.view] === "albums") {
    resetNav();
    await openAlbumsView();
    return;
  }

  markNavActive("library");
  renderSort(null);
  showPanels({ tracks: true });
  if (!hasBrowseFilter()) {
    showAllTracks();
    return;
  }
  els.viewTitle.textContent = browseTitle(state.browseFilter);
  streamTracks((offset) => tracksApi(state.browseFilter, offset));
}

/// Switch the center panel between the library and the history views. The library
/// view restores whatever the browse filter currently resolves to; the history
/// views fetch their tracks fresh each time so they reflect the latest listens.
function markNavActive(view) {
  state.view = view;
  const seg = SEG_FOR_VIEW[view] || null;
  for (const button of els.segButtons) {
    button.classList.toggle("is-active", button.dataset.view === seg);
  }
  for (const link of els.navLinks) {
    link.classList.toggle("is-active", link.dataset.view === view);
  }
}

const HISTORY_VIEWS = {
  most: {
    url: "/api/history/most-played",
    title: "Most played",
    empty: "Nothing played yet.",
    annotate: (track) => `${track.play_count} ${track.play_count === 1 ? "play" : "plays"}`,
  },
  recent: {
    url: "/api/history/recent",
    title: "Recently played",
    empty: "Nothing played yet.",
    annotate: (track) => relativeTime(track.last_listened_at),
  },
};

// Recently played is a cheap query, so we keep it live while it's open; most played
// is a full aggregation, so we only load it on demand (and the server caches it).
const RECENT_REFRESH_INTERVAL = 5000;
let recentRefreshTimer = 0;

async function setView(view) {
  // The segmented tabs and sidebar nav are roots, so a fresh stack and no Back button.
  resetNav();
  clearInterval(recentRefreshTimer);
  recentRefreshTimer = 0;
  state.currentPlaylistId = null;
  state.currentSmartId = null;
  renderPlaylistsSidebar();
  renderSmartPlaylistsSidebar();

  if (view === "albums") {
    await openAlbumsView();
    return;
  }
  if (view === "artists") {
    await openArtistsView();
    return;
  }

  markNavActive(view);
  renderSort(null);
  showPanels({ tracks: true });

  if (view === "library") {
    await applyBrowseFilter({ clearSearch: false });
    return;
  }
  if (view === "favorites") {
    await openFavoritesView();
    return;
  }

  await loadHistoryView(view, { showLoading: true });
  if (view === "recent") {
    recentRefreshTimer = setInterval(() => loadHistoryView("recent"), RECENT_REFRESH_INTERVAL);
  }
}

// Fetch and render a history view. Safe to call repeatedly so the list stays live
// without any manual refresh.
async function loadHistoryView(view, { showLoading = false } = {}) {
  const config = HISTORY_VIEWS[view];
  if (!config) return;
  els.viewTitle.textContent = config.title;
  if (showLoading) renderLoading(6);
  try {
    const tracks = await api(config.url);
    if (state.view !== view) return; // The user switched away while we were loading.
    state.visibleTracks = tracks;
    if (tracks.length === 0) {
      els.trackList.innerHTML = `<p class="empty">${config.empty}</p>`;
    } else {
      renderTracks(tracks, { annotate: config.annotate });
    }
  } catch (error) {
    els.trackList.innerHTML = `<p class="error">Failed to load history: ${escapeHtml(error.message)}</p>`;
  }
}

function currentBrowseFilter() {
  return {
    genre: els.browseGenre.value,
    year: els.browseYear.value,
    composer: els.browseComposer.value,
  };
}

function clearBrowseFilter() {
  state.browseFilter = { genre: "", year: "", composer: "" };
  renderBrowseFilters();
}

function hasBrowseFilter(filter = state.browseFilter) {
  return Boolean(filter.genre || filter.year || filter.composer);
}

function browseTitle(filter) {
  const parts = cleanParts([filter.genre, filter.year, filter.composer]);
  return `Browse: ${parts.join(" / ")}`;
}

// A shimmering placeholder list shown while a fetch is in flight.
function renderLoading(count = 8) {
  let rows = "";
  for (let i = 0; i < count; i += 1) {
    const width = 30 + ((i * 37) % 50);
    rows += `<div class="skeleton-row"><span class="skeleton-bar" style="--w:${width}%"></span></div>`;
  }
  els.trackList.innerHTML = `<div class="skeleton" aria-hidden="true">${rows}</div>`;
}

// Render the center track list. `options.annotate(track)` may return a short string
// (a play count, a relative time) shown as a stat at the end of each row.
// Render a finite, fully-known track list (album detail, playlist, favorites,
// history). For the large, open-ended views (all tracks, browse, search) use
// `streamTracks`, which pages in on scroll.
function renderTracks(tracks, options = {}) {
  state.trackStream?.destroy();
  state.trackStream = null;
  state.visibleTracks = tracks;
  els.trackList.innerHTML = "";

  if (tracks.length === 0) {
    els.trackList.innerHTML = `<p class="empty">No tracks found.</p>`;
    return;
  }

  for (const [index, track] of tracks.entries()) {
    els.trackList.append(buildTrackRow(track, index, options));
  }

  markActiveTrack();
  markMetadataTrack();
}

// Infinite-scroll a large track view. `getPage(offset)` returns the next page of
// tracks; rows append as you scroll and `state.visibleTracks` grows with them (so
// row index — used by playTrack — stays absolute). `state.tracks` aliases the same
// growing array for callers that read "the current list".
function streamTracks(getPage, options = {}) {
  state.trackStream?.destroy();
  state.visibleTracks = [];
  state.tracks = state.visibleTracks;
  els.trackList.innerHTML = "";
  let received = false;

  state.trackStream = infiniteScroll(
    els.trackList,
    getPage,
    (batch, offset) => {
      if (!received && offset === 0 && batch.length === 0) {
        els.trackList.innerHTML = `<p class="empty">No tracks found.</p>`;
        return;
      }
      received = true;
      for (let i = 0; i < batch.length; i++) {
        const track = batch[i];
        state.visibleTracks.push(track);
        els.trackList.append(buildTrackRow(track, offset + i, options));
      }
      markActiveTrack();
      markMetadataTrack();
    },
  );
}

// The default center view: every track, newest pages fetched on scroll.
function showAllTracks() {
  els.viewTitle.textContent = "Tracks";
  streamTracks((offset) => tracksApi({}, offset));
}

function buildTrackRow(track, index, options = {}) {
  const row = document.createElement("div");
  row.className = "track";
  row.dataset.trackId = track.id;

  const playButton = document.createElement("button");
  playButton.className = "track-main";
  playButton.type = "button";
  playButton.innerHTML = `
    <span>
      <strong>${escapeHtml(track.title)}</strong>
      <span>${escapeHtml(track.artist_name)}</span>
    </span>
    <span>${escapeHtml(track.album_title)}</span>
    <small>${track.extension.toUpperCase()}</small>
  `;
  playButton.addEventListener("click", () => playTrack(index));

  const stat = options.annotate ? options.annotate(track) : null;
  let statEl = null;
  if (stat) {
    statEl = document.createElement("small");
    statEl.className = "track-stat";
    statEl.textContent = stat;
  }

  const actions = document.createElement("div");
  actions.className = "track-actions";

  // Favorite heart.
  const heart = document.createElement("button");
  heart.type = "button";
  heart.className = "icon-toggle heart";
  heart.title = "Favorite";
  const favored = state.favoriteTrackIds.has(track.id);
  heart.classList.toggle("on", favored);
  heart.textContent = favored ? "♥" : "♡";
  heart.setAttribute("aria-pressed", String(favored));
  heart.addEventListener("click", () => toggleFavorite(track.id, heart));

  // Context action: remove (in a playlist view) or add-to-playlist (elsewhere).
  const context = document.createElement("button");
  context.type = "button";
  context.className = "icon-toggle";
  if (options.playlistId) {
    context.textContent = "✕";
    context.title = "Remove from playlist";
    context.addEventListener("click", () => removeFromPlaylist(options.playlistId, index));
  } else {
    context.textContent = "＋";
    context.title = "Add to playlist";
    context.addEventListener("click", (event) => {
      event.stopPropagation();
      openAddToPlaylist([track.id], context, `Add “${track.title}” to…`);
    });
  }

  const metadataButton = document.createElement("button");
  metadataButton.className = "track-action";
  metadataButton.type = "button";
  metadataButton.textContent = "Metadata";
  metadataButton.addEventListener("click", () => openMetadata(track.id, track));

  actions.append(heart, context, metadataButton);

  if (statEl) {
    row.append(playButton, statEl, actions);
  } else {
    row.append(playButton, actions);
  }
  return row;
}

// ---- Favorites + playlists ----

async function loadFavoriteIds() {
  try {
    const favorites = await api("/api/favorites");
    state.favoriteTrackIds = new Set((favorites.tracks || []).map((track) => track.id));
  } catch {
    /* keep whatever we had */
  }
}

// Reflect the favorite set onto already-rendered heart buttons.
function refreshHearts() {
  for (const row of els.trackList.querySelectorAll(".track")) {
    const heart = row.querySelector(".heart");
    if (!heart) continue;
    const on = state.favoriteTrackIds.has(row.dataset.trackId);
    heart.classList.toggle("on", on);
    heart.textContent = on ? "♥" : "♡";
    heart.setAttribute("aria-pressed", String(on));
  }
}

async function toggleFavorite(trackId, button) {
  const on = state.favoriteTrackIds.has(trackId);
  try {
    await apiJson(`/api/favorites/track/${encodeURIComponent(trackId)}`, on ? "DELETE" : "PUT");
  } catch {
    return;
  }
  if (on) state.favoriteTrackIds.delete(trackId);
  else state.favoriteTrackIds.add(trackId);
  if (button) {
    const now = !on;
    button.classList.toggle("on", now);
    button.textContent = now ? "♥" : "♡";
    button.setAttribute("aria-pressed", String(now));
  }
  if (state.view === "favorites" && on) openFavoritesView();
}

async function openFavoritesView() {
  markNavActive("favorites");
  state.currentPlaylistId = null;
  renderPlaylistsSidebar();
  els.viewTitle.textContent = "Favorites";
  renderLoading(6);
  try {
    const favorites = await api("/api/favorites");
    state.favoriteTrackIds = new Set((favorites.tracks || []).map((track) => track.id));
    state.visibleTracks = favorites.tracks;
    if (favorites.tracks.length === 0) {
      els.trackList.innerHTML = `<p class="empty">No favorites yet — tap ♡ on a track.</p>`;
    } else {
      renderTracks(favorites.tracks);
    }
  } catch (error) {
    els.trackList.innerHTML = `<p class="error">Failed to load favorites: ${escapeHtml(error.message)}</p>`;
  }
}

async function loadPlaylists() {
  try {
    state.playlists = await api("/api/playlists");
  } catch {
    state.playlists = [];
  }
  renderPlaylistsSidebar();
}

// The smart-playlist catalog is fixed (computed views over history/favorites); fetch it
// once and render it as a read-only sidebar list.
async function loadSmartPlaylists() {
  try {
    state.smartPlaylists = await api("/api/smart-playlists");
  } catch {
    state.smartPlaylists = [];
  }
  renderSmartPlaylistsSidebar();
}

function renderSmartPlaylistsSidebar() {
  els.smartPlaylists.innerHTML = "";
  if (!state.smartPlaylists.length) return;
  for (const smart of state.smartPlaylists) {
    const row = document.createElement("div");
    row.className =
      "playlist-item" + (smart.id === state.currentSmartId ? " is-active" : "");
    const open = document.createElement("button");
    open.type = "button";
    open.className = "pl-open";
    open.title = smart.description;
    open.innerHTML = `<span class="pl-name">${escapeHtml(smart.name)}</span>`;
    open.addEventListener("click", () => openSmartPlaylistView(smart.id));
    row.append(open);
    els.smartPlaylists.append(row);
  }
}

// Open a computed playlist: same master view as a user playlist, but read-only and
// fetched live from /api/smart-playlists/{id}.
async function openSmartPlaylistView(id) {
  markNavActive("smart");
  resetNav();
  renderSort(null);
  showPanels({ tracks: true });
  state.currentPlaylistId = null;
  state.currentSmartId = id;
  renderPlaylistsSidebar();
  renderSmartPlaylistsSidebar();
  closeDrawerOnMobile();
  renderLoading(6);
  try {
    const detail = await api(`/api/smart-playlists/${encodeURIComponent(id)}`);
    if (state.currentSmartId !== id) return; // switched away while loading
    state.visibleTracks = detail.tracks;
    els.viewTitle.textContent = detail.name;
    if (!detail.tracks.length) {
      els.trackList.innerHTML = `<p class="empty">${escapeHtml(detail.description)}<br>Nothing here yet.</p>`;
    } else {
      renderTracks(detail.tracks);
    }
  } catch (error) {
    els.trackList.innerHTML = `<p class="error">Failed to load: ${escapeHtml(error.message)}</p>`;
  }
}

function renderPlaylistsSidebar() {
  els.playlists.innerHTML = "";
  if (!state.playlists.length) {
    els.playlists.innerHTML = `<p class="muted-hint">No playlists yet.</p>`;
    return;
  }
  for (const playlist of state.playlists) {
    const row = document.createElement("div");
    row.className = "playlist-item" + (playlist.id === state.currentPlaylistId ? " is-active" : "");

    const open = document.createElement("button");
    open.type = "button";
    open.className = "pl-open";
    open.innerHTML = `<span class="pl-name">${escapeHtml(playlist.name)}</span><span class="pl-count">${playlist.song_count}</span>`;
    open.addEventListener("click", () => openPlaylistView(playlist.id));

    const del = document.createElement("button");
    del.type = "button";
    del.className = "pl-del";
    del.title = "Delete playlist";
    del.textContent = "✕";
    del.addEventListener("click", (event) => {
      event.stopPropagation();
      deletePlaylist(playlist.id, playlist.name);
    });

    row.append(open, del);
    els.playlists.append(row);
  }
}

async function openPlaylistView(id) {
  markNavActive("playlist");
  resetNav();
  renderSort(null);
  showPanels({ tracks: true });
  state.currentPlaylistId = id;
  state.currentSmartId = null;
  renderPlaylistsSidebar();
  renderSmartPlaylistsSidebar();
  closeDrawerOnMobile();
  renderLoading(6);
  try {
    const detail = await api(`/api/playlists/${encodeURIComponent(id)}`);
    state.visibleTracks = detail.tracks;
    els.viewTitle.textContent = detail.name;
    if (detail.tracks.length === 0) {
      els.trackList.innerHTML = `<p class="empty">This playlist is empty — add tracks with ＋.</p>`;
    } else {
      renderTracks(detail.tracks, { playlistId: id });
    }
  } catch (error) {
    els.trackList.innerHTML = `<p class="error">Failed to load playlist: ${escapeHtml(error.message)}</p>`;
  }
}

async function createPlaylist(name, trackIds) {
  const body = { name };
  if (trackIds) body.track_ids = trackIds;
  const detail = await apiJson("/api/playlists", "POST", body);
  await loadPlaylists();
  return detail;
}

async function addTracksToPlaylist(playlistId, trackIds) {
  await apiJson(`/api/playlists/${encodeURIComponent(playlistId)}`, "PATCH", {
    add_track_ids: trackIds,
  });
  await loadPlaylists();
  // If the playlist being added to is the one on screen, reflect it.
  if (state.currentPlaylistId === playlistId) openPlaylistView(playlistId);
}

async function removeFromPlaylist(playlistId, index) {
  try {
    await apiJson(`/api/playlists/${encodeURIComponent(playlistId)}`, "PATCH", {
      remove_indices: [index],
    });
    openPlaylistView(playlistId);
  } catch {
    /* ignore */
  }
}

async function deletePlaylist(id, name) {
  const ok = await confirmAction({
    title: "Delete playlist",
    message: `Delete the playlist “${name}”? This can’t be undone.`,
  });
  if (!ok) return;
  try {
    await apiJson(`/api/playlists/${encodeURIComponent(id)}`, "DELETE");
  } catch {
    return;
  }
  if (state.currentPlaylistId === id) {
    state.currentPlaylistId = null;
    setView("library");
  }
  await loadPlaylists();
}

// Popover to add one or more tracks (a song, or a whole album) to a playlist. The
// optional `heading` labels what's being added.
function openAddToPlaylist(trackIds, anchor, heading) {
  closeAddMenu();
  if (!trackIds.length) return;
  const menu = document.createElement("div");
  menu.className = "add-menu";

  const label = document.createElement("div");
  label.className = "add-menu-head";
  label.textContent =
    heading || (trackIds.length === 1 ? "Add to playlist" : `Add ${trackIds.length} tracks to…`);
  menu.append(label);

  for (const playlist of state.playlists) {
    const item = document.createElement("button");
    item.type = "button";
    item.className = "add-menu-item";
    item.textContent = playlist.name;
    item.addEventListener("click", async () => {
      closeAddMenu();
      await addTracksToPlaylist(playlist.id, trackIds);
    });
    menu.append(item);
  }
  const create = document.createElement("button");
  create.type = "button";
  create.className = "add-menu-item new";
  create.textContent = "New playlist…";
  create.addEventListener("click", async () => {
    closeAddMenu();
    const name = await promptText({
      title: "New playlist",
      label: "Playlist name",
      placeholder: "My playlist",
      confirmLabel: "Create",
    });
    if (name) await createPlaylist(name, trackIds);
  });
  menu.append(create);

  document.body.append(menu);
  const rect = anchor.getBoundingClientRect();
  menu.style.top = `${rect.bottom + 4}px`;
  menu.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - 228))}px`;
  state.addMenu = menu;
  setTimeout(() => document.addEventListener("click", onAddMenuOutside), 0);
}

function closeAddMenu() {
  if (state.addMenu) {
    state.addMenu.remove();
    state.addMenu = null;
    document.removeEventListener("click", onAddMenuOutside);
  }
}

function onAddMenuOutside(event) {
  if (state.addMenu && !state.addMenu.contains(event.target)) closeAddMenu();
}

// ---- Internet radio ----

async function loadRadio() {
  // Read stations through the radio source's generic browse endpoint (the same
  // provider abstraction local disk and SMB use). Browse entries carry the
  // station's stream URL, so they're directly playable. Adding/removing stations
  // still goes through /api/radio.
  try {
    const { entries } = await api(`/api/sources/radio/browse`);
    state.radio = (entries || []).map((entry) => ({
      id: entry.id,
      name: entry.title,
      stream_url: entry.stream_url,
      homepage_url: entry.homepage_url,
    }));
  } catch {
    state.radio = [];
  }
  renderRadioSidebar();
}

function renderRadioSidebar() {
  els.radio.innerHTML = "";
  if (!state.radio.length) {
    els.radio.innerHTML = `<p class="muted-hint">No stations yet.</p>`;
    return;
  }
  for (const station of state.radio) {
    const row = document.createElement("div");
    row.className = "playlist-item";

    const open = document.createElement("button");
    open.type = "button";
    open.className = "pl-open";
    open.innerHTML = `<span class="pl-name">${escapeHtml(station.name)}</span><span class="pl-count">▶</span>`;
    open.title = "Play on the active player";
    open.addEventListener("click", () => playRadio(station));

    const del = document.createElement("button");
    del.type = "button";
    del.className = "pl-del";
    del.title = "Remove station";
    del.textContent = "✕";
    del.addEventListener("click", (event) => {
      event.stopPropagation();
      deleteRadio(station.id, station.name);
    });

    row.append(open, del);
    els.radio.append(row);
  }
}

function playRadio(station) {
  if (!state.activePlayerId) return;
  if (browserOutputsFor(state.activePlayerId)) {
    claimBrowserOutput();
    // Start within the user gesture so the browser's autoplay policy allows it.
    if (browserAudio) {
      browserAudio.src = station.stream_url;
      browserAudio.play().catch(() => {});
    }
  }
  commandTarget({
    command: "play_stream",
    url: station.stream_url,
    title: station.name,
  });
  closeDrawerOnMobile();
}

async function deleteRadio(id, name) {
  const ok = await confirmAction({
    title: "Remove station",
    message: `Remove the radio station “${name}”?`,
    confirmLabel: "Remove",
  });
  if (!ok) return;
  try {
    await apiJson(`/api/radio/${encodeURIComponent(id)}`, "DELETE");
  } catch {
    return;
  }
  await loadRadio();
}

// ---- Radio directory (Radio Browser) ----

function openRadioDirectory() {
  els.radioDirectory.hidden = false;
  els.radioSearch.value = "";
  searchRadioDirectory("");
  els.radioSearch.focus();
}

function closeRadioDirectory() {
  els.radioDirectory.hidden = true;
}

async function searchRadioDirectory(query) {
  els.radioResults.innerHTML = `<p class="muted-hint">Loading…</p>`;
  try {
    const stations = await api(`/api/radio/directory?query=${encodeURIComponent(query)}`);
    renderRadioResults(stations);
  } catch {
    els.radioResults.innerHTML = `<p class="error">Couldn't reach the radio directory.</p>`;
  }
}

function renderRadioResults(stations) {
  els.radioResults.innerHTML = "";
  if (!stations.length) {
    els.radioResults.innerHTML = `<p class="muted-hint">No stations found.</p>`;
    return;
  }
  for (const station of stations) {
    const meta = [
      station.country,
      station.codec ? `${station.codec}${station.bitrate ? ` ${station.bitrate}k` : ""}` : null,
      station.tags,
    ]
      .filter(Boolean)
      .join(" · ");
    const row = document.createElement("div");
    row.className = "radio-result";
    row.innerHTML = `
      <span class="rr-text">
        <strong>${escapeHtml(station.name)}</strong>
        <span>${escapeHtml(meta)}</span>
      </span>
    `;
    const play = document.createElement("button");
    play.type = "button";
    play.className = "ghost-button mini";
    play.title = "Play now";
    play.textContent = "▶";
    play.addEventListener("click", () => playRadio(station));

    const add = document.createElement("button");
    add.type = "button";
    add.className = "ghost-button mini";
    add.title = "Add to my stations";
    add.textContent = "＋";
    add.addEventListener("click", async () => {
      add.disabled = true;
      try {
        await apiJson("/api/radio", "POST", {
          name: station.name,
          stream_url: station.stream_url,
          homepage_url: station.homepage_url ?? null,
        });
        await loadRadio();
        add.textContent = "✓";
      } catch {
        add.disabled = false;
      }
    });

    row.append(play, add);
    els.radioResults.append(row);
  }
}

// Compact relative time like "just now", "5m", "3h", "2d" from a Unix timestamp.
function relativeTime(unixSeconds) {
  if (!unixSeconds) return "";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000 - unixSeconds));
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

// Play the current track list on the active player, starting at `index`.
// Play an arbitrary list of track ids on the active output, starting at `startIndex`.
// When this tab is the output, start the first track's audio *within the user gesture*
// (so the browser's autoplay policy allows it); the server round-trip then syncs state.
// One play_tracks sets the whole queue AND the start position, avoiding a wrong-track
// flicker that a play_tracks + play_queue_index two-step would cause.
function playTrackIds(ids, startIndex = 0, startStreamUrl = null) {
  if (!ids.length || !state.activePlayerId) return Promise.resolve();
  if (browserOutputsFor(state.activePlayerId)) {
    claimBrowserOutput();
    if (startStreamUrl) {
      els.audio.src = startStreamUrl;
      els.audio.play().catch(() => {});
    }
  }
  return commandTarget({ command: "play_tracks", track_ids: ids, start_index: startIndex });
}

async function playTrack(index) {
  const tracks = state.visibleTracks;
  if (tracks.length === 0) return;
  await playTrackIds(
    tracks.map((track) => track.id),
    index,
    tracks[index]?.stream_url || null,
  );
}

function markActiveTrack() {
  for (const row of els.trackList.querySelectorAll(".track")) {
    row.classList.toggle(
      "active",
      state.activeNowTrackId != null && row.dataset.trackId === state.activeNowTrackId,
    );
  }
}

function markMetadataTrack() {
  for (const row of els.trackList.querySelectorAll(".track")) {
    const selected = row.dataset.trackId === state.metadataTrackId;
    row.classList.toggle("metadata-open", selected);
    row.querySelector(".track-action")?.classList.toggle("active", selected);
  }
}

async function metadataReviewApi(trackId) {
  return api(`/api/tracks/${encodeURIComponent(trackId)}/metadata/review`);
}

async function updateMetadataField(trackId, field, approvalState) {
  return apiPatch(`/api/tracks/${encodeURIComponent(trackId)}/metadata/review/fields`, {
    source: field.source,
    observed_at_unix_seconds: field.observed_at_unix_seconds,
    field_name: field.field_name,
    value: field.value,
    approval_state: approvalState,
  });
}

async function trackCandidatesApi(trackId) {
  return api(`/api/tracks/${encodeURIComponent(trackId)}/metadata/musicbrainz/candidates?limit=5`);
}

async function albumCandidatesApi(albumId) {
  return api(`/api/albums/${encodeURIComponent(albumId)}/metadata/musicbrainz/candidates?limit=5`);
}

async function albumArtworkReviewApi(albumId) {
  return api(`/api/albums/${encodeURIComponent(albumId)}/artwork/review`);
}

async function coverArtArchiveCandidatesApi(albumId) {
  return api(`/api/albums/${encodeURIComponent(albumId)}/artwork/cover-art-archive/candidates?limit=10`);
}

async function selectAlbumArtworkApi(albumId, artworkId) {
  return apiPatch(`/api/albums/${encodeURIComponent(albumId)}/artwork`, {
    artwork_id: artworkId,
  });
}

async function openMetadata(trackId, track = null) {
  // Metadata and the queue share the right-rail top; only one shows at a time.
  if (state.queueOpen) toggleQueue(false);
  state.metadataTrackId = trackId;
  state.metadataTrackObj = track;
  state.metadataReview = null;
  state.metadataError = "";
  state.metadataTrackCandidates = null;
  state.metadataAlbumCandidates = null;
  state.metadataCandidateLoading = "";
  state.metadataArtworkReview = null;
  state.metadataArtworkError = "";
  state.metadataArtworkLoading = false;
  state.metadataCoverArtCandidates = null;
  state.metadataCoverArtError = "";
  state.metadataCoverArtLoading = false;
  markMetadataTrack();
  renderMetadataPanel();

  try {
    const review = await metadataReviewApi(trackId);
    if (state.metadataTrackId !== trackId) {
      return;
    }
    state.metadataReview = review;
  } catch (error) {
    if (state.metadataTrackId === trackId) {
      state.metadataError = `Metadata review failed: ${error.message}`;
    }
  } finally {
    if (state.metadataTrackId === trackId) {
      renderMetadataPanel();
    }
  }

  const current = metadataTrack();
  if (state.metadataTrackId === trackId && current?.album_id) {
    loadAlbumArtworkReview(current.album_id, trackId);
  }
}

function closeMetadata() {
  state.metadataTrackId = null;
  state.metadataTrackObj = null;
  state.metadataReview = null;
  state.metadataError = "";
  state.metadataTrackCandidates = null;
  state.metadataAlbumCandidates = null;
  state.metadataCandidateLoading = "";
  state.metadataArtworkReview = null;
  state.metadataArtworkError = "";
  state.metadataArtworkLoading = false;
  state.metadataCoverArtCandidates = null;
  state.metadataCoverArtError = "";
  state.metadataCoverArtLoading = false;
  markMetadataTrack();
  renderMetadataPanel();
}

function renderMetadataPanel() {
  const track = metadataTrack();
  els.metadataPanel.classList.toggle("open", Boolean(state.metadataTrackId));
  els.metadataTitle.textContent = track?.title || "No track selected";
  els.metadataBody.innerHTML = "";

  if (!state.metadataTrackId) {
    els.metadataBody.append(element("p", "empty", "No track selected."));
    return;
  }

  if (!track) {
    els.metadataBody.append(element("p", "error", "Selected track is no longer in the library."));
    return;
  }

  if (state.metadataError) {
    els.metadataBody.append(element("p", "error", state.metadataError));
  }

  if (!state.metadataReview) {
    els.metadataBody.append(element("p", "empty", "Loading metadata."));
    return;
  }

  els.metadataBody.append(renderCanonicalMetadata(state.metadataReview.canonical));
  els.metadataBody.append(renderArtworkReview(track));
  els.metadataBody.append(renderObservedMetadata(state.metadataReview.observations));
  els.metadataBody.append(renderMusicBrainzCandidates(track));
}

function renderCanonicalMetadata(canonical) {
  const section = metadataSection("Canonical");
  appendMetadataRow(section, "Title", canonical.title);
  appendMetadataRow(section, "Artist", canonical.artist_name);
  appendMetadataRow(section, "Album", canonical.album_title);
  appendMetadataRow(section, "Year", canonical.year);
  appendMetadataRow(section, "Track", canonical.track_number);
  return section;
}

function renderArtworkReview(track) {
  const section = metadataSection("Album artwork");
  const actions = element("div", "candidate-actions");
  const coverArtButton = document.createElement("button");
  coverArtButton.type = "button";
  coverArtButton.textContent = state.metadataCoverArtLoading
    ? "Loading Cover Art Archive"
    : "Cover Art Archive";
  coverArtButton.disabled = state.metadataCoverArtLoading;
  coverArtButton.addEventListener("click", () => loadCoverArtArchiveCandidates(track.album_id));
  actions.append(coverArtButton);
  section.append(actions);

  if (state.metadataArtworkLoading) {
    section.append(element("p", "empty", "Loading artwork."));
  }

  if (state.metadataArtworkError) {
    section.append(element("p", "error", state.metadataArtworkError));
  }
  if (state.metadataCoverArtError) {
    section.append(element("p", "error", state.metadataCoverArtError));
  }

  const review = state.metadataArtworkReview;
  const coverArt = state.metadataCoverArtCandidates;
  if (!review && !state.metadataArtworkLoading && !coverArt) {
    section.append(element("p", "empty", "No artwork review."));
    return section;
  }

  const candidates = review?.candidates || [];
  if (candidates.length === 0 && !state.metadataArtworkLoading && !coverArt) {
    section.append(element("p", "empty", "No local artwork candidates."));
    return section;
  }

  for (const candidate of candidates) {
    section.append(renderArtworkCandidate(track.album_id, candidate));
  }
  if (coverArt) {
    section.append(renderCoverArtArchiveResponse(coverArt));
  }

  return section;
}

function renderArtworkCandidate(albumId, candidate) {
  const row = element("div", "artwork-row");

  if (candidate.preview_url) {
    const image = document.createElement("img");
    image.className = "artwork-thumb";
    image.src = candidate.preview_url;
    image.alt = "";
    row.append(image);
  } else {
    row.append(element("span", "artwork-placeholder", ""));
  }

  const details = element("div", "artwork-details");
  details.append(element("strong", "", candidate.file_name));

  const meta = element("div", "metadata-source");
  meta.append(
    element("span", "", candidate.mime_type),
    element("span", "", bytesText(candidate.file_size_bytes)),
    element("span", candidate.selected ? "metadata-state state-approved" : "metadata-state", candidate.selected ? "Selected" : "Available"),
  );
  details.append(meta);

  const actions = element("div", "metadata-actions");
  const button = document.createElement("button");
  button.type = "button";
  button.textContent = candidate.selected ? "Selected" : "Select";
  button.disabled = candidate.selected || state.metadataArtworkLoading;
  button.classList.toggle("active", candidate.selected);
  button.addEventListener("click", () => selectAlbumArtwork(albumId, candidate.id));
  actions.append(button);
  details.append(actions);

  row.append(details);
  return row;
}

function renderCoverArtArchiveResponse(response) {
  const result = element("div", "candidate-result");
  result.append(element("h3", "", "Cover Art Archive"));

  if (response.skipped_reason) {
    result.append(element("p", "empty", response.skipped_reason));
  }
  for (const issue of response.issues || []) {
    result.append(element("p", "error", issue.message));
  }

  const candidates = response.candidates || [];
  if (candidates.length === 0) {
    result.append(element("p", "empty", "No Cover Art Archive candidates."));
    return result;
  }

  for (const candidate of candidates) {
    result.append(renderCoverArtArchiveCandidate(candidate));
  }

  return result;
}

function renderCoverArtArchiveCandidate(candidate) {
  const row = element("div", "artwork-row");
  const image = document.createElement("img");
  image.className = "artwork-thumb";
  image.src = candidate.thumbnail_url || candidate.image_url;
  image.alt = "";
  row.append(image);

  const details = element("div", "artwork-details");
  details.append(element("strong", "", coverArtArchiveLabel(candidate)));

  const meta = element("div", "metadata-source");
  meta.append(
    element("span", "", metadataLabel(candidate.entity_type)),
    element("span", "", candidate.approved ? "Approved" : "Unapproved"),
    element("span", "", candidate.front ? "Front" : candidate.back ? "Back" : "Other"),
  );
  details.append(meta);

  const actions = element("div", "metadata-actions");
  const link = document.createElement("a");
  link.href = candidate.image_url;
  link.target = "_blank";
  link.rel = "noreferrer";
  link.textContent = "Open";
  actions.append(link);
  details.append(actions);

  row.append(details);
  return row;
}

function coverArtArchiveLabel(candidate) {
  if (candidate.comment) {
    return candidate.comment;
  }
  if (candidate.front) {
    return "Front cover";
  }
  if (candidate.back) {
    return "Back cover";
  }
  return "Artwork";
}

async function loadAlbumArtworkReview(albumId, trackId = state.metadataTrackId) {
  state.metadataArtworkLoading = true;
  state.metadataArtworkError = "";
  renderMetadataPanel();

  try {
    const review = await albumArtworkReviewApi(albumId);
    if (state.metadataTrackId === trackId && metadataTrack()?.album_id === albumId) {
      state.metadataArtworkReview = review;
    }
  } catch (error) {
    if (state.metadataTrackId === trackId) {
      state.metadataArtworkError = `Artwork review failed: ${error.message}`;
    }
  } finally {
    if (state.metadataTrackId === trackId) {
      state.metadataArtworkLoading = false;
      renderMetadataPanel();
    }
  }
}

async function loadCoverArtArchiveCandidates(albumId) {
  const trackId = state.metadataTrackId;
  state.metadataCoverArtLoading = true;
  state.metadataCoverArtError = "";
  renderMetadataPanel();

  try {
    const response = await coverArtArchiveCandidatesApi(albumId);
    if (state.metadataTrackId === trackId && metadataTrack()?.album_id === albumId) {
      state.metadataCoverArtCandidates = response;
    }
  } catch (error) {
    if (state.metadataTrackId === trackId) {
      state.metadataCoverArtError = `Cover Art Archive failed: ${error.message}`;
    }
  } finally {
    if (state.metadataTrackId === trackId) {
      state.metadataCoverArtLoading = false;
      renderMetadataPanel();
    }
  }
}

async function selectAlbumArtwork(albumId, artworkId) {
  const trackId = state.metadataTrackId;
  state.metadataArtworkLoading = true;
  state.metadataArtworkError = "";
  renderMetadataPanel();

  try {
    const review = await selectAlbumArtworkApi(albumId, artworkId);
    if (state.metadataTrackId === trackId && metadataTrack()?.album_id === albumId) {
      state.metadataArtworkReview = review;
      updateAlbumArtwork(albumId, review.selected_artwork_url);
    }
  } catch (error) {
    if (state.metadataTrackId === trackId) {
      state.metadataArtworkError = `Artwork selection failed: ${error.message}`;
    }
  } finally {
    if (state.metadataTrackId === trackId) {
      state.metadataArtworkLoading = false;
      renderMetadataPanel();
    }
  }
}

function updateAlbumArtwork(albumId, artworkUrl) {
  const album = state.albums.find((item) => item.id === albumId);
  if (album) {
    album.artwork_url = artworkUrl;
  }
  // Patch the rendered card in place (the album browse grid), if it's currently shown.
  if (!artworkUrl) {
    return;
  }
  const card = els.browseGrid.querySelector(`[data-album-id="${CSS.escape(albumId)}"]`);
  if (!card) {
    return;
  }
  let img = card.querySelector("img");
  if (!img) {
    img = document.createElement("img");
    img.alt = "";
    img.loading = "lazy";
    card.querySelector(".album-placeholder")?.replaceWith(img);
  }
  img.src = sizedArtwork(artworkUrl, 300);
}

function renderObservedMetadata(observations) {
  const section = metadataSection("Observed fields");

  if (observations.length === 0) {
    section.append(element("p", "empty", "No observed metadata."));
    return section;
  }

  for (const observation of observations) {
    const header = element("div", "metadata-source");
    header.append(
      element("strong", "", observation.source),
      element("span", "", confidenceText(observation.confidence)),
      element("span", stateClass(observation.approval_state), stateLabel(observation.approval_state)),
    );
    section.append(header);

    for (const field of observation.fields) {
      section.append(renderMetadataField(field));
    }
  }

  return section;
}

function renderMetadataField(field) {
  const row = element("div", "metadata-field");
  row.append(element("span", "", metadataLabel(field.field_name)));

  const value = element("div", "metadata-value");
  value.append(element("strong", "", fieldValueText(field.value)));

  const meta = element("div", "metadata-source");
  meta.append(
    element("span", "", field.source),
    element("span", "", confidenceText(field.confidence)),
    element("span", stateClass(field.approval_state), stateLabel(field.approval_state)),
  );
  value.append(meta);

  const actions = element("div", "metadata-actions");
  for (const approvalState of ["observed", "approved", "rejected"]) {
    const button = document.createElement("button");
    button.className = approvalState;
    button.type = "button";
    button.textContent = stateActionLabel(approvalState);
    button.classList.toggle("active", field.approval_state === approvalState);
    button.addEventListener("click", () => setFieldApproval(field, approvalState));
    actions.append(button);
  }
  value.append(actions);

  row.append(value);
  return row;
}

async function setFieldApproval(field, approvalState) {
  const trackId = state.metadataTrackId;
  if (!trackId) {
    return;
  }

  try {
    state.metadataError = "";
    const review = await updateMetadataField(trackId, field, approvalState);
    if (state.metadataTrackId === trackId) {
      state.metadataReview = review;
    }
  } catch (error) {
    if (state.metadataTrackId === trackId) {
      state.metadataError = `Metadata update failed: ${error.message}`;
    }
  } finally {
    if (state.metadataTrackId === trackId) {
      renderMetadataPanel();
    }
  }
}

function renderMusicBrainzCandidates(track) {
  const section = metadataSection("MusicBrainz candidates");
  const actions = element("div", "candidate-actions");
  const trackButton = document.createElement("button");
  trackButton.type = "button";
  trackButton.textContent = state.metadataCandidateLoading === "track"
    ? "Loading track"
    : "Track candidates";
  trackButton.disabled = Boolean(state.metadataCandidateLoading);
  trackButton.addEventListener("click", loadTrackCandidates);

  const albumButton = document.createElement("button");
  albumButton.type = "button";
  albumButton.textContent = state.metadataCandidateLoading === "album"
    ? "Loading album"
    : "Album candidates";
  albumButton.disabled = Boolean(state.metadataCandidateLoading || !track.album_id);
  albumButton.addEventListener("click", loadAlbumCandidates);

  actions.append(trackButton, albumButton);
  section.append(actions);

  if (state.metadataTrackCandidates) {
    section.append(renderCandidateResponse(state.metadataTrackCandidates, "track"));
  }
  if (state.metadataAlbumCandidates) {
    section.append(renderCandidateResponse(state.metadataAlbumCandidates, "album"));
  }

  return section;
}

async function loadTrackCandidates() {
  const trackId = state.metadataTrackId;
  if (!trackId) {
    return;
  }

  state.metadataCandidateLoading = "track";
  renderMetadataPanel();
  try {
    const response = await trackCandidatesApi(trackId);
    if (state.metadataTrackId === trackId) {
      state.metadataTrackCandidates = response;
    }
  } catch (error) {
    if (state.metadataTrackId === trackId) {
      state.metadataTrackCandidates = candidateError(error);
    }
  } finally {
    if (state.metadataTrackId === trackId) {
      state.metadataCandidateLoading = "";
      renderMetadataPanel();
    }
  }
}

async function loadAlbumCandidates() {
  const track = metadataTrack();
  if (!track?.album_id) {
    return;
  }

  const trackId = state.metadataTrackId;
  const albumId = track.album_id;
  state.metadataCandidateLoading = "album";
  renderMetadataPanel();
  try {
    const response = await albumCandidatesApi(albumId);
    if (state.metadataTrackId === trackId) {
      state.metadataAlbumCandidates = response;
    }
  } catch (error) {
    if (state.metadataTrackId === trackId) {
      state.metadataAlbumCandidates = candidateError(error);
    }
  } finally {
    if (state.metadataTrackId === trackId) {
      state.metadataCandidateLoading = "";
      renderMetadataPanel();
    }
  }
}

function renderCandidateResponse(response, type) {
  const result = element("div", "candidate-result");
  result.append(element("h3", "", type === "track" ? "Track matches" : "Album matches"));

  if (response.error) {
    result.append(element("p", "error", response.error));
    return result;
  }

  if (response.query) {
    appendMetadataRow(result, "Query", response.query);
  }
  if (response.skipped_reason) {
    result.append(element("p", "empty", response.skipped_reason));
  }
  for (const issue of response.issues || []) {
    result.append(element("p", "error", issue.message));
  }

  const candidates = response.candidates || [];
  if (candidates.length === 0) {
    result.append(element("p", "empty", `No ${type} candidates.`));
    return result;
  }

  for (const candidate of candidates) {
    result.append(renderCandidate(candidate, type));
  }

  return result;
}

function renderCandidate(candidate, type) {
  const row = element("div", "candidate-row");
  row.append(element("strong", "", candidate.title || "Untitled"));

  const meta = type === "track"
    ? [
        artistCreditText(candidate.artist_credit),
        candidate.first_release_date,
        durationText(candidate.length_ms),
        candidate.releases?.length ? `${candidate.releases.length} releases` : "",
        scoreText(candidate.score),
      ]
    : [
        artistCreditText(candidate.artist_credit),
        candidate.date,
        candidate.country,
        candidate.status,
        candidate.media?.length ? `${candidate.media.length} media` : "",
        scoreText(candidate.score),
      ];
  row.append(element("span", "candidate-meta", cleanParts(meta).join(" - ")));

  if (candidate.disambiguation) {
    row.append(element("span", "candidate-meta", candidate.disambiguation));
  }

  const link = document.createElement("a");
  link.href = `https://musicbrainz.org/${type === "track" ? "recording" : "release"}/${encodeURIComponent(candidate.id)}`;
  link.target = "_blank";
  link.rel = "noreferrer";
  link.textContent = candidate.id;
  row.append(link);

  return row;
}

// The track whose metadata panel is open. We stash the row's track object when the
// panel opens (it may scroll out of the loaded window), falling back to the visible
// list if the stash is stale.
function metadataTrack() {
  if (state.metadataTrackObj && state.metadataTrackObj.id === state.metadataTrackId) {
    return state.metadataTrackObj;
  }
  return state.visibleTracks.find((track) => track.id === state.metadataTrackId);
}

function metadataSection(title) {
  const section = element("section", "metadata-section");
  section.append(element("h3", "", title));
  return section;
}

function appendMetadataRow(parent, label, value) {
  const row = element("div", "metadata-row");
  row.append(
    element("span", "", label),
    element("strong", "", value === null || value === undefined || value === "" ? "Unknown" : value),
  );
  parent.append(row);
}

function fieldValueText(value) {
  if (!value) {
    return "Unknown";
  }

  if (value.kind === "text_list") {
    return Array.isArray(value.value) && value.value.length > 0 ? value.value.join(", ") : "None";
  }

  if (["text", "number", "count"].includes(value.kind)) {
    return value.value === null || value.value === undefined || value.value === "" ? "Unknown" : value.value;
  }

  return JSON.stringify(value);
}

function metadataLabel(value) {
  return String(value)
    .replaceAll("_", " ")
    .replace(/\b\w/g, (letter) => letter.toUpperCase());
}

function stateLabel(value) {
  return metadataLabel(value || "observed");
}

function stateActionLabel(value) {
  if (value === "approved") {
    return "Approve";
  }
  if (value === "rejected") {
    return "Reject";
  }
  return "Observe";
}

function stateClass(value) {
  return `metadata-state state-${value || "observed"}`;
}

function confidenceText(value) {
  return `${Math.round(Number(value || 0) * 100)}%`;
}

function artistCreditText(value) {
  return Array.isArray(value) ? value.join(", ") : "";
}

function scoreText(value) {
  return value === null || value === undefined ? "" : `score ${value}`;
}

function durationText(lengthMs) {
  if (!lengthMs) {
    return "";
  }
  const seconds = Math.round(lengthMs / 1000);
  const minutes = Math.floor(seconds / 60);
  return `${minutes}:${String(seconds % 60).padStart(2, "0")}`;
}

function bytesText(value) {
  if (!value) {
    return "0 B";
  }
  if (value < 1024) {
    return `${value} B`;
  }
  if (value < 1024 * 1024) {
    return `${Math.round(value / 102.4) / 10} KB`;
  }
  return `${Math.round(value / 1024 / 102.4) / 10} MB`;
}

function cleanParts(values) {
  return values.filter((value) => value !== null && value !== undefined && value !== "");
}

function candidateError(error) {
  return {
    error: `Candidate search failed: ${error.message}`,
    candidates: [],
    issues: [],
  };
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) {
    node.className = className;
  }
  if (text !== undefined) {
    node.textContent = text;
  }
  return node;
}

async function search() {
  const query = els.search.value.trim();
  if (!query) {
    state.searchController?.abort();
    state.searchController = null;
    await applyBrowseFilter({ clearSearch: false });
    return;
  }

  if (hasBrowseFilter()) {
    clearBrowseFilter();
  }

  state.searchController?.abort();
  const controller = new AbortController();
  state.searchController = controller;

  try {
    // The first page also carries matching albums; later pages just extend the track
    // results as you scroll.
    const first = await searchApi(query, controller.signal, 0);
    if (state.searchController !== controller) {
      return;
    }

    markNavActive("library");
    resetNav();
    renderSort(null);
    const albums = first.albums || [];
    showPanels({ grid: albums.length > 0, tracks: true });
    els.viewTitle.textContent = `Search: ${query}`;
    if (albums.length) {
      streamBrowseGrid((offset) => albums.slice(offset, offset + ALBUM_PAGE), buildAlbumCard);
    }
    streamTracks((offset) =>
      offset === 0
        ? first.tracks
        : searchApi(query, controller.signal, offset).then((page) => page.tracks),
    );
  } catch (error) {
    if (error.name === "AbortError") {
      return;
    }
    els.trackList.innerHTML = `<p class="error">Search failed: ${escapeHtml(error.message)}</p>`;
  }
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
els.browseGenre.addEventListener("change", () => applyBrowseFilter());
els.browseYear.addEventListener("change", () => applyBrowseFilter());
els.browseComposer.addEventListener("change", () => applyBrowseFilter());
els.browseClear.addEventListener("click", () => {
  clearBrowseFilter();
  applyBrowseFilter();
});
for (const link of els.navLinks) {
  link.addEventListener("click", () => {
    setView(link.dataset.view);
    closeDrawerOnMobile();
  });
}
for (const button of els.segButtons) {
  button.addEventListener("click", () => setView(button.dataset.view));
}
els.backBtn.addEventListener("click", () => history.back());
els.sortSelect.addEventListener("change", () => {
  if (state.view === "albums") {
    state.albumSort = els.sortSelect.value;
    openAlbumsView();
  } else if (state.view === "artists") {
    state.artistSort = els.sortSelect.value;
    openArtistsView();
  }
});
// The browser Back button (and our ‹ Back) pop the in-app browse stack.
window.addEventListener("popstate", () => {
  if (state.navStack.length) popNav();
});
// Create-playlist uses an inline input rather than window.prompt(): prompt() is
// suppressed in installed/standalone PWAs and on mobile, where this controller is
// meant to run, so the button did nothing there. The input also lets us show errors.
function toggleNewPlaylist(open) {
  const show = open ?? els.newPlaylistForm.hidden;
  els.newPlaylistForm.hidden = !show;
  els.newPlaylist.setAttribute("aria-expanded", String(show));
  els.newPlaylistError.hidden = true;
  if (show) {
    els.newPlaylistName.value = "";
    els.newPlaylistName.focus();
  }
}

els.newPlaylist.addEventListener("click", () => toggleNewPlaylist());

els.newPlaylistForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const name = els.newPlaylistName.value.trim();
  if (!name) return;
  els.newPlaylistName.disabled = true;
  try {
    const detail = await createPlaylist(name);
    toggleNewPlaylist(false);
    if (detail) openPlaylistView(detail.id);
  } catch (error) {
    els.newPlaylistError.textContent = `Couldn't create playlist: ${error.message}`;
    els.newPlaylistError.hidden = false;
  } finally {
    els.newPlaylistName.disabled = false;
  }
});

els.newPlaylistName.addEventListener("keydown", (event) => {
  if (event.key === "Escape") toggleNewPlaylist(false);
});
els.newRadio.addEventListener("click", openRadioDirectory);
els.radioDirClose.addEventListener("click", closeRadioDirectory);
els.radioDirectory.addEventListener("click", (event) => {
  if (event.target === els.radioDirectory) closeRadioDirectory();
});
els.radioSearch.addEventListener("input", debounce(() => searchRadioDirectory(els.radioSearch.value.trim()), 250));
els.radioAddUrl.addEventListener("click", async () => {
  const values = await openModal({
    title: "Add radio station",
    fields: [
      { key: "name", label: "Station name", placeholder: "BBC Radio 6" },
      { key: "url", label: "Stream URL", placeholder: "https://…", type: "url" },
    ],
    confirmLabel: "Add",
  });
  if (!values || !values.name || !values.url) return;
  try {
    await apiJson("/api/radio", "POST", { name: values.name, stream_url: values.url });
    await loadRadio();
    closeRadioDirectory();
  } catch (error) {
    showToast(error.message || "Couldn’t add the station.");
  }
});
// Auto-refresh the library: poll for filesystem changes and re-check on focus, so
// there is no manual refresh button.
const LIBRARY_SYNC_INTERVAL = 20000;
setInterval(syncLibrary, LIBRARY_SYNC_INTERVAL);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") syncLibrary();
});
window.addEventListener("focus", syncLibrary);
els.metadataClose.addEventListener("click", closeMetadata);
// Footer transport targets the active player.
els.prev.addEventListener("click", () => {
  if (state.activePlayerId) commandTarget({ command: "previous" });
});
els.next.addEventListener("click", () => {
  if (state.activePlayerId) commandTarget({ command: "next" });
});
els.playPause.addEventListener("click", () => {
  if (!state.activePlayerId) return;
  if (state.activeStatus === "playing") {
    commandTarget({ command: "pause" });
  } else if (state.activeNowTrackId) {
    if (browserOutputsFor(state.activePlayerId)) claimBrowserOutput();
    commandTarget({ command: "play" });
  } else {
    // Nothing queued yet: start the current track list.
    playTrack(0);
  }
});

// Seek: preview while dragging, commit on release.
els.seek.addEventListener("input", () => {
  state.seekDragging = true;
  const value = Number(els.seek.value);
  els.elapsed.textContent = formatTime(value);
  els.seek.style.setProperty(
    "--fill",
    `${state.activeDuration > 0 ? (value / state.activeDuration) * 100 : 0}%`,
  );
});
els.seek.addEventListener("change", () => {
  const value = Number(els.seek.value);
  state.seekDragging = false;
  state.activeElapsed = value;
  if (state.activePlayerId) {
    commandTarget({ command: "seek", position_seconds: value });
  }
});

els.shuffle.addEventListener("click", () => {
  if (!state.activePlayerId) return;
  const enabled = !(state.activeState?.shuffle ?? false);
  commandTarget({ command: "set_shuffle", enabled });
});

els.repeat.addEventListener("click", () => {
  if (!state.activePlayerId) return;
  const order = ["off", "all", "one"];
  const current = state.activeState?.repeat ?? "off";
  const mode = order[(order.indexOf(current) + 1) % order.length];
  commandTarget({ command: "set_repeat", mode });
});

let footerVolumeTimer = 0;
els.footerVolume.addEventListener("input", () => {
  const volume = Number.parseInt(els.footerVolume.value, 10);
  els.footerVolume.style.setProperty("--fill", `${volume}%`);
  if (browserOutputsFor(state.activePlayerId) && browserOutput) {
    els.audio.volume = Math.min(1, Math.max(0, volume / 100));
  }
  clearTimeout(footerVolumeTimer);
  footerVolumeTimer = setTimeout(() => {
    if (state.activePlayerId) {
      commandTarget({ command: "set_volume", volume });
    }
  }, 150);
});

// Queue drawer.
function toggleQueue(open) {
  state.queueOpen = open ?? !state.queueOpen;
  els.queueDrawer.hidden = !state.queueOpen;
  els.queueToggle.setAttribute("aria-pressed", String(state.queueOpen));
  if (state.queueOpen) {
    renderQueue();
    if (state.activeState) state.lastQueueSig = queueSignature(state.activeState);
  }
}

els.queueToggle.addEventListener("click", () => toggleQueue());
els.queueClose.addEventListener("click", () => toggleQueue(false));
els.queueClear.addEventListener("click", () => {
  if (state.activePlayerId) commandTarget({ command: "clear" });
});

function renderQueue() {
  const playback = state.activeState;
  const queue = playback?.queue ?? [];
  const position = playback?.queue_position ?? -1;
  if (queue.length === 0) {
    els.queueList.innerHTML = `<p class="queue-empty">The queue is empty.</p>`;
    return;
  }
  els.queueList.innerHTML = queue
    .map((item, index) => {
      const art = item.artwork_url
        ? `<span class="q-art"><img src="${escapeHtml(item.artwork_url)}" alt=""></span>`
        : `<span class="q-art">${escapeHtml((item.title || "♪").trim().charAt(0).toUpperCase())}</span>`;
      return `
        <div class="queue-row ${index === position ? "current" : ""}" data-index="${index}">
          <span class="q-index">${index === position ? "▶" : index + 1}</span>
          ${art}
          <button class="q-main" data-action="play-index" type="button">
            <span class="q-title">${escapeHtml(item.title || "Unknown")}</span>
            <span class="q-sub">${escapeHtml([item.artist, item.album].filter(Boolean).join(" · "))}</span>
          </button>
          <span class="q-actions">
            <button class="icon-button" data-action="up" title="Move up" ${index === 0 ? "disabled" : ""}>↑</button>
            <button class="icon-button" data-action="down" title="Move down" ${index === queue.length - 1 ? "disabled" : ""}>↓</button>
            <button class="icon-button" data-action="remove" title="Remove">&times;</button>
          </span>
        </div>`;
    })
    .join("");
}

els.queueList.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-action]");
  if (!button || !state.activePlayerId) return;
  const index = Number(button.closest("[data-index]")?.dataset.index);
  if (Number.isNaN(index)) return;
  const action = button.dataset.action;
  if (action === "play-index") {
    if (browserOutputsFor(state.activePlayerId)) claimBrowserOutput();
    commandTarget({ command: "play_queue_index", index });
  } else if (action === "remove") {
    commandTarget({ command: "remove_queue_item", index });
  } else if (action === "up") {
    commandTarget({ command: "move_queue_item", from: index, to: index - 1 });
  } else if (action === "down") {
    commandTarget({ command: "move_queue_item", from: index, to: index + 1 });
  }
});

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

renderLoading();
loadLibrary();
loadPlaylists();
loadSmartPlaylists();
loadRadio();
loadFavoriteIds().then(refreshHearts);

function debounce(fn, delay) {
  let timer = 0;
  return (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), delay);
  };
}

// ---- Players (active-output switcher) -------------------------------------
// Player/zone/source MANAGEMENT lives on the /admin page; here we only need the
// active-output switcher and live playback state.

const playerSockets = new Map();
let playerData = { players: [] };

async function apiJson(path, method, body) {
  const init = { method };
  if (body !== undefined) {
    init.headers = { "Content-Type": "application/json" };
    init.body = JSON.stringify(body);
  }
  const response = await fetch(path, init);
  if (!response.ok) {
    // Surface the server's error message (`{ error: { message } }`) rather than a
    // bare status code, so failures like a bad SMB host are actually readable.
    let detail = `${response.status} ${response.statusText}`;
    try {
      const payload = await response.json();
      if (payload?.error?.message) detail = payload.error.message;
    } catch {
      /* non-JSON body; keep the status line */
    }
    throw new Error(detail);
  }
  return response.status === 204 ? null : response.json();
}

async function loadPlayers() {
  let players;
  let zones;
  try {
    [players, zones] = await Promise.all([api("/api/players"), api("/api/zones")]);
  } catch (error) {
    console.warn("players unavailable", error);
    return;
  }
  playerData = { players };
  state.zones = zones || [];
  browserPlayerId = players.find((player) => player.kind === "browser")?.id ?? null;
  renderPlayerSwitcher(players);

  // One socket per control target (player or zone) for live state. Drop sockets for
  // targets that no longer exist.
  const targetIds = new Set([...players.map((p) => p.id), ...state.zones.map((z) => z.id)]);
  for (const [id, socket] of playerSockets) {
    if (!targetIds.has(id)) {
      socket.close();
      playerSockets.delete(id);
    }
  }
  for (const id of targetIds) {
    if (!playerSockets.has(id)) {
      openPlayerSocket(id);
    }
  }
}

function openPlayerSocket(id) {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const base = isZoneTarget(id) ? "zones" : "players";
  let socket;
  try {
    socket = new WebSocket(`${scheme}://${location.host}/api/${base}/${encodeURIComponent(id)}/ws`);
  } catch {
    return;
  }
  socket.onmessage = (event) => {
    try {
      const playback = JSON.parse(event.data);
      // Lightweight position tick (browser player, ~1×/second): only the elapsed
      // position changed, so update the seek readout without touching the queue,
      // cover art, OS metadata, or the track-list highlight.
      if (playback.type === "progress") {
        if (id === state.activePlayerId) applyProgressTick(playback);
        return;
      }
      // Track every player's status so the switcher can flag which are playing.
      state.playerStatus[id] = playback.status;
      // When any player advances to a different track, a listen for the previous one
      // may have just been recorded — refresh an open history view so it stays live.
      const nowTrack = playback.now_playing?.track_id ?? null;
      if (nowTrack !== state.lastNow[id]) {
        state.lastNow[id] = nowTrack;
        // Recently played is cheap and should feel live; most played is a heavier
        // aggregation we only refresh on demand.
        if (state.view === "recent") {
          loadHistoryView("recent");
        }
      }
      // Render audio only off the active target's socket, and only if this tab is
      // that target's legitimate output (the browser player, or a zone it's in).
      if (browserOutput && id === state.activePlayerId && browserOutputsFor(id)) {
        driveBrowserAudio(playback);
      }
      if (id === state.activePlayerId) {
        updateFooterFromState(playback);
      } else {
        updateSwitchIndicator();
      }
      if (state.playerMenuOpen) renderPlayerSwitcher(playerData.players);
    } catch {
      /* ignore malformed frames */
    }
  };
  socket.onclose = () => handlePlayerSocketClosed(id);
  // An error fires before close on a failed/lost connection; force the close path.
  socket.onerror = () => {
    try {
      socket.close();
    } catch {
      /* already closing */
    }
  };
  playerSockets.set(id, socket);
}

// The player WebSocket dropped (server down or restarting). If this tab was rendering
// the browser player's audio, stop it — don't keep playing a stream the server can no
// longer control. Then try to reconnect so controls recover when the server returns.
function handlePlayerSocketClosed(id) {
  playerSockets.delete(id);

  // If the socket driving this tab's audio (the active target's) dropped, stop —
  // don't keep playing a stream the server can no longer control.
  if (
    id === state.activePlayerId &&
    browserOutput &&
    browserOutputsFor(id) &&
    browserAudio &&
    !browserAudio.paused
  ) {
    browserAudio.pause();
    state.playerStatus[id] = "stopped";
    state.activeStatus = "stopped";
    els.playPause.textContent = "▶";
    els.miniPlay.textContent = "▶";
    els.transport.dataset.status = "stopped";
    updateSwitchIndicator();
  }

  scheduleSocketReconnect(id);
}

function scheduleSocketReconnect(id) {
  setTimeout(() => {
    const stillRegistered =
      (playerData.players || []).some((player) => player.id === id) || isZoneTarget(id);
    if (stillRegistered && !playerSockets.has(id)) {
      openPlayerSocket(id);
    }
  }, 3000);
}

// ---- Browser-player audio output ------------------------------------------

// The browser player renders through the footer audio element.
const browserAudio = els.audio;
const TAB_ID = Math.random().toString(36).slice(2);
let browserPlayerId = null;
let browserOutput = false;
let browserProgressTimer = 0;

// Make this tab the audio output for the browser player, releasing any other tab.
function claimBrowserOutput() {
  browserOutput = true;
  try {
    localStorage.setItem("musicata-output", TAB_ID);
  } catch {
    /* localStorage may be unavailable */
  }
}

// Only one tab plays at a time: a newer claim elsewhere makes this tab let go.
window.addEventListener("storage", (event) => {
  if (event.key === "musicata-output" && event.newValue !== TAB_ID) {
    browserOutput = false;
    if (browserAudio) browserAudio.pause();
  }
});

function driveBrowserAudio(playback) {
  if (!browserAudio) return;
  if (playback.volume != null) {
    browserAudio.volume = Math.min(1, Math.max(0, playback.volume / 100));
  }
  const now = playback.now_playing;
  if (playback.status === "playing" && now && now.stream_url) {
    if (!browserAudio.src.endsWith(now.stream_url)) {
      browserAudio.src = now.stream_url;
    }
    // Apply an external seek (a large jump), ignoring our own progress echoes.
    const elapsed = playback.elapsed_seconds ?? 0;
    if (Math.abs(browserAudio.currentTime - elapsed) > 2) {
      browserAudio.currentTime = elapsed;
    }
    browserAudio.play().catch(() => {});
  } else if (playback.status === "paused") {
    browserAudio.pause();
  } else {
    browserAudio.pause();
  }
}

// Report progress/ended back to whichever target owns the audio this tab renders —
// the active target's socket (a zone when outputting for a zone, else the browser
// player), so the server-owned queue advances and elapsed stays live.
function browserSocketSend(message) {
  const targetId = state.activePlayerId;
  if (!targetId || !browserOutputsFor(targetId)) return;
  const socket = playerSockets.get(targetId);
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(message));
  }
}

function reportBrowserProgress() {
  if (!browserOutput) return;
  const duration = Number.isFinite(browserAudio.duration) ? browserAudio.duration : null;
  browserSocketSend({
    type: "progress",
    elapsed_seconds: browserAudio.currentTime,
    duration_seconds: duration,
  });
}

if (browserAudio) {
  browserAudio.addEventListener("ended", () => {
    if (browserOutput) browserSocketSend({ type: "ended" });
  });
  // Report duration as soon as it's known so the seek bar gets a real length.
  browserAudio.addEventListener("loadedmetadata", reportBrowserProgress);
  browserProgressTimer = setInterval(() => {
    if (browserOutput && !browserAudio.paused) reportBrowserProgress();
  }, 1000);
}

function nowPlayingText(playback) {
  const icon =
    playback.status === "playing" ? "▶" : playback.status === "paused" ? "⏸" : "■";
  const now = playback.now_playing;
  if (!now) {
    return `${icon} ${playback.status}`;
  }
  const artist = now.artist ? ` — ${now.artist}` : "";
  return `${icon} ${now.title || "Unknown"}${artist}`;
}

function cssEscape(value) {
  return value.replace(/["\\]/g, "\\$&");
}

let toastTimer = null;
// Brief, self-dismissing status message (bottom-center). Replaces any visible toast
// so repeated failures don't stack.
function showToast(message) {
  if (!els.toast) return;
  els.toast.textContent = message;
  els.toast.hidden = false;
  // Reflow so re-showing the same toast restarts the fade-in transition.
  void els.toast.offsetWidth;
  els.toast.classList.add("show");
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    els.toast.classList.remove("show");
    toastTimer = setTimeout(() => {
      els.toast.hidden = true;
    }, 250);
  }, 3500);
}

// --- In-product dialogs (never window.confirm/prompt — those are suppressed in PWAs) ---

// A modal with a title, optional message, optional fields, and confirm/cancel. Resolves
// to the field values ({} when none) on confirm, or null on cancel. Esc / scrim / Cancel
// cancel; Enter confirms. Restores focus to the previously-focused element.
function openModal({ title, message, fields = [], confirmLabel = "OK", danger = false }) {
  return new Promise((resolve) => {
    const previousFocus = document.activeElement;
    const scrim = document.createElement("div");
    scrim.className = "modal-scrim";
    const dialog = document.createElement("form");
    dialog.className = "modal";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");

    if (title) {
      const heading = document.createElement("h2");
      heading.className = "modal-title";
      heading.textContent = title;
      dialog.append(heading);
    }
    if (message) {
      const body = document.createElement("p");
      body.className = "modal-message";
      body.textContent = message;
      dialog.append(body);
    }

    const inputs = new Map();
    if (fields.length) {
      const list = document.createElement("div");
      list.className = "modal-fields";
      for (const field of fields) {
        const label = document.createElement("label");
        label.className = "field";
        const span = document.createElement("span");
        span.textContent = field.label;
        const input = document.createElement("input");
        input.type = field.type || "text";
        input.value = field.value || "";
        if (field.placeholder) input.placeholder = field.placeholder;
        if (field.maxLength) input.maxLength = field.maxLength;
        input.autocomplete = "off";
        label.append(span, input);
        list.append(label);
        inputs.set(field.key, input);
      }
      dialog.append(list);
    }

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "ghost-button";
    cancel.textContent = "Cancel";
    const confirm = document.createElement("button");
    confirm.type = "submit";
    confirm.className = danger ? "ghost-button danger" : "primary-button";
    confirm.textContent = confirmLabel;
    actions.append(cancel, confirm);
    dialog.append(actions);
    scrim.append(dialog);
    document.body.append(scrim);

    const close = (result) => {
      scrim.remove();
      document.removeEventListener("keydown", onKey);
      if (previousFocus && previousFocus.focus) previousFocus.focus();
      resolve(result);
    };
    const onKey = (event) => {
      if (event.key === "Escape") close(null);
    };
    const values = () => {
      const out = {};
      for (const [key, input] of inputs) out[key] = input.value.trim();
      return out;
    };
    dialog.addEventListener("submit", (event) => {
      event.preventDefault();
      close(fields.length ? values() : {});
    });
    cancel.addEventListener("click", () => close(null));
    scrim.addEventListener("click", (event) => {
      if (event.target === scrim) close(null);
    });
    document.addEventListener("keydown", onKey);
    requestAnimationFrame(() => {
      const first = fields.length ? inputs.values().next().value : confirm;
      first.focus();
    });
  });
}

// Destructive-action confirmation. Returns true if confirmed.
async function confirmAction({ title, message, confirmLabel = "Delete", danger = true }) {
  const result = await openModal({ title, message, confirmLabel, danger });
  return result !== null;
}

// Single-field text prompt. Returns the trimmed value, or null if cancelled/empty.
async function promptText({ title, label, value = "", placeholder, confirmLabel = "Save" }) {
  const result = await openModal({
    title,
    fields: [{ key: "value", label, value, placeholder }],
    confirmLabel,
  });
  if (result === null) return null;
  return result.value || null;
}

async function playerCommand(id, command) {
  try {
    await apiJson(`${targetBase(id)}${encodeURIComponent(id)}/commands`, "POST", command);
  } catch (error) {
    // A command can fail because the output is offline (e.g. MPD is down). The track
    // is still queued server-side; tell the user rather than failing silently.
    console.error("player command failed", error);
    showToast(error.message || "Couldn't reach the player.");
  }
}

// A control target is either a player or a zone; a zone owns a canonical queue and
// is controlled through the parallel `/api/zones/...` surface. These helpers route
// by target so the transport, queue, and play actions work the same for both.
function isZoneTarget(id) {
  return state.zones.some((zone) => zone.id === id);
}

function targetBase(id) {
  return isZoneTarget(id) ? "/api/zones/" : "/api/players/";
}

// Send a command to the currently active target (player or zone).
function commandTarget(command) {
  if (!state.activePlayerId) return Promise.resolve();
  return playerCommand(state.activePlayerId, command);
}

function browserPlayerRecord() {
  return playerData.players.find((player) => player.id === browserPlayerId);
}

// Whether this browser tab is the legitimate audio output for a target: the browser
// player itself, or a zone the browser player is a member of (zones drive their
// browser members' audio off the zone state, not the member's own queue).
function browserOutputsFor(targetId) {
  if (!targetId) return false;
  if (targetId === browserPlayerId) return true;
  if (isZoneTarget(targetId)) {
    const record = browserPlayerRecord();
    return !!record && record.zone_id === targetId;
  }
  return false;
}

// ---- Active player (footer + main list target) ----------------------------

function savedActivePlayer() {
  try {
    return localStorage.getItem("musicata-active-player");
  } catch {
    return null;
  }
}

// Dot class for a player's live status: playing > online > offline.
function dotClass(player) {
  const status = state.playerStatus[player.id];
  if (status === "playing") return "pm-dot playing";
  if (player.online) return "pm-dot online";
  return "pm-dot";
}

function menuItem(id, name, dot) {
  const active = id === state.activePlayerId;
  return `
        <button class="player-menu-item ${active ? "active" : ""}"
                data-player="${escapeHtml(id)}" type="button" role="menuitem">
          <span class="${dot}"></span>
          <span class="pm-name">${escapeHtml(name)}</span>
          <span class="pm-check">${active ? "✓" : ""}</span>
        </button>`;
}

// A zone is a control target alongside players; it's "online" as long as it exists,
// "playing" when its canonical queue is.
function zoneDotClass(zone) {
  return state.playerStatus[zone.id] === "playing" ? "pm-dot playing" : "pm-dot online";
}

function renderPlayerSwitcher(players) {
  if (!els.playerMenuList) return;
  let html = players.map((player) => menuItem(player.id, player.name, dotClass(player))).join("");
  if (state.zones.length) {
    html += `<div class="pm-group" role="presentation">Zones</div>`;
    html += state.zones.map((zone) => menuItem(zone.id, zone.name, zoneDotClass(zone))).join("");
  }
  els.playerMenuList.innerHTML = html;

  const known =
    players.some((player) => player.id === state.activePlayerId) ||
    state.zones.some((zone) => zone.id === state.activePlayerId);
  if (!known) {
    const saved = savedActivePlayer();
    const active =
      players.find((player) => player.id === saved)?.id ||
      state.zones.find((zone) => zone.id === saved)?.id ||
      browserPlayerId ||
      players[0]?.id ||
      null;
    if (active) setActivePlayer(active);
  } else {
    updateSwitchIndicator();
  }
}

function setActivePlayer(id) {
  if (!id) return;
  state.activePlayerId = id;
  // Force the next state update to repaint everything for the new player (its track,
  // toggles, and queue differ), bypassing the change-gating in updateFooterFromState.
  state.lastTrackKey = undefined;
  state.lastStatus = undefined;
  state.lastShuffle = undefined;
  state.lastRepeat = undefined;
  state.lastQueueSig = undefined;
  try {
    localStorage.setItem("musicata-active-player", id);
  } catch {
    /* ignore */
  }
  if (!browserOutputsFor(id)) {
    // A remote player (or a zone this browser isn't in) renders its own audio;
    // this tab must stay silent.
    els.audio.pause();
  }
  renderPlayerSwitcher(playerData.players);
  refreshActivePlayerFooter();
}

// Reflect the active target's (player or zone) name + live status on the button.
function updateSwitchIndicator() {
  const player = playerData.players.find((entry) => entry.id === state.activePlayerId);
  const zone = state.zones.find((entry) => entry.id === state.activePlayerId);
  els.switchName.textContent = player ? player.name : zone ? zone.name : "No player";
  const here = browserOutput && browserOutputsFor(state.activePlayerId);
  const playing = state.playerStatus[state.activePlayerId] === "playing";
  els.switchSignal.className = "signal";
  if (playing || here) els.switchSignal.classList.add("playing");
  else if (player?.online || zone) els.switchSignal.classList.add("online");
}

function setPlayerMenu(open) {
  state.playerMenuOpen = open;
  els.playerMenu.hidden = !open;
  els.switchBtn.setAttribute("aria-expanded", String(open));
  if (open) renderPlayerSwitcher(playerData.players);
}

els.switchBtn.addEventListener("click", (event) => {
  event.stopPropagation();
  setPlayerMenu(!state.playerMenuOpen);
});
els.playerMenuList.addEventListener("click", (event) => {
  const item = event.target.closest("[data-player]");
  if (!item) return;
  setActivePlayer(item.dataset.player);
  setPlayerMenu(false);
});
els.playerMenuConfig.addEventListener("click", () => {
  setPlayerMenu(false);
  openAdmin();
});
document.addEventListener("click", (event) => {
  if (state.playerMenuOpen && !event.target.closest(".player-switch")) {
    setPlayerMenu(false);
  }
});

async function refreshActivePlayerFooter() {
  if (!state.activePlayerId) return;
  try {
    const playback = await api(
      `${targetBase(state.activePlayerId)}${encodeURIComponent(state.activePlayerId)}/state`,
    );
    updateFooterFromState(playback);
  } catch {
    state.activeStatus = "stopped";
    state.activeNowTrackId = null;
    els.playPause.textContent = "Play";
    markActiveTrack();
  }
}

function formatTime(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
  const total = Math.floor(seconds);
  const minutes = Math.floor(total / 60);
  const secs = String(total % 60).padStart(2, "0");
  return `${minutes}:${secs}`;
}

function setRange(input, value, max) {
  input.max = String(max > 0 ? max : 0);
  input.value = String(Math.min(value, max || 0));
  const pct = max > 0 ? (Math.min(value, max) / max) * 100 : 0;
  input.style.setProperty("--fill", `${pct}%`);
}

// A cheap fingerprint of the queue, so we can skip rebuilding the drawer when the
// queue is unchanged (the common case on a per-second progress tick).
function queueSignature(playback) {
  const queue = playback.queue || [];
  let sig = `${queue.length}:${playback.queue_position ?? ""}`;
  for (const item of queue) sig += `|${item.track_id ?? item.stream_url ?? item.title}`;
  return sig;
}

// Apply a lightweight position tick from the active player: refresh the seek bar,
// the elapsed/duration text, and the OS scrubber — nothing else. Keeps the cached
// activeState's position in sync so local smoothing continues from the right place.
function applyProgressTick(tick) {
  state.activeElapsed = tick.elapsed_seconds ?? 0;
  if (tick.duration_seconds != null) state.activeDuration = tick.duration_seconds;
  if (state.activeState) {
    state.activeState.elapsed_seconds = state.activeElapsed;
    if (tick.duration_seconds != null) state.activeState.duration_seconds = state.activeDuration;
  }
  if (!state.seekDragging) {
    setRange(els.seek, state.activeElapsed, state.activeDuration);
    els.elapsed.textContent = formatTime(state.activeElapsed);
    els.duration.textContent = formatTime(state.activeDuration);
  }
  updateMediaSessionPosition({
    elapsed_seconds: state.activeElapsed,
    duration_seconds: state.activeDuration,
  });
}

function updateFooterFromState(playback) {
  state.activeState = playback;
  state.activeStatus = playback.status;
  const now = playback.now_playing;
  state.activeNowTrackId = now?.track_id ?? null;
  state.activeElapsed = playback.elapsed_seconds ?? 0;
  state.activeDuration = playback.duration_seconds ?? 0;

  // While a track plays the server broadcasts the full state every second, but only
  // the elapsed position changes. Recompute the expensive parts — cover art, OS media
  // metadata, the active-row highlight (a sweep over the whole track list), and the
  // queue drawer — only when their inputs actually change; otherwise these per-second
  // DOM passes peg the main thread and the controls feel sluggish.
  const trackKey = now ? (now.track_id ?? now.stream_url ?? now.title ?? "") : null;
  const trackChanged = trackKey !== state.lastTrackKey;
  const statusChanged = playback.status !== state.lastStatus;
  const shuffle = Boolean(playback.shuffle);
  const repeat = playback.repeat || "off";

  // Cheap, every tick: transport status, play/pause glyphs, volume, seek position.
  els.transport.dataset.status = playback.status;
  els.playPause.textContent = playback.status === "playing" ? "❚❚" : "▶";
  els.playPause.title = playback.status === "playing" ? "Pause" : "Play";
  els.miniPlay.textContent = playback.status === "playing" ? "❚❚" : "▶";

  if (playback.volume != null && document.activeElement !== els.footerVolume) {
    setRange(els.footerVolume, playback.volume, 100);
  }
  if (!state.seekDragging) {
    setRange(els.seek, state.activeElapsed, state.activeDuration);
    els.elapsed.textContent = formatTime(state.activeElapsed);
    els.duration.textContent = formatTime(state.activeDuration);
  }
  updateMediaSessionPosition(playback);

  // Shuffle + repeat toggles: only when they flip.
  if (shuffle !== state.lastShuffle) {
    els.shuffle.classList.toggle("active", shuffle);
    els.shuffle.setAttribute("aria-pressed", String(shuffle));
    state.lastShuffle = shuffle;
  }
  if (repeat !== state.lastRepeat) {
    els.repeat.dataset.mode = repeat;
    els.repeat.classList.toggle("active", repeat !== "off");
    els.repeat.innerHTML = repeat === "one" ? "↻<span class='one'>1</span>" : "↻";
    state.lastRepeat = repeat;
  }

  // Track-dependent work: only when the now-playing track changes.
  if (trackChanged) {
    renderNowArt(now);
    if (now) {
      els.nowTitle.textContent = now.title || "Unknown";
      els.nowSubtitle.textContent = [now.artist, now.album].filter(Boolean).join(" · ");
    } else {
      els.nowTitle.textContent = "Nothing playing";
      els.nowSubtitle.textContent =
        state.activePlayerId === browserPlayerId
          ? "Pick a track to play here."
          : "Select a track to play on this player.";
    }
    markActiveTrack();
    state.lastTrackKey = trackKey;
  }
  if (trackChanged || statusChanged) {
    updateMediaSessionMetadata(playback);
    state.lastStatus = playback.status;
  }

  updateSwitchIndicator();

  // Queue drawer: rebuild only when open and the queue actually changed — a string
  // signature compare is far cheaper than rebuilding the DOM every second.
  els.queueCount.textContent = String(playback.queue?.length ?? 0);
  if (state.queueOpen) {
    const sig = queueSignature(playback);
    if (sig !== state.lastQueueSig) {
      renderQueue();
      state.lastQueueSig = sig;
    }
  }
}

// Reflect the active player's track into the OS media surfaces (lock screen, media
// keys, notification, Bluetooth/car displays). Constructing a MediaMetadata makes the
// browser (re)fetch the artwork, so this runs only when the track or status changes —
// not on every progress tick. The OS UI only appears once this tab is actually
// producing audio, but setting metadata for any active player is harmless.
function updateMediaSessionMetadata(playback) {
  if (!("mediaSession" in navigator)) return;
  const ms = navigator.mediaSession;

  ms.playbackState =
    playback.status === "playing"
      ? "playing"
      : playback.status === "paused"
        ? "paused"
        : "none";

  const now = playback.now_playing;
  if (now) {
    ms.metadata = new MediaMetadata({
      title: now.title || "Unknown",
      artist: now.artist || "",
      album: now.album || "",
      artwork: now.artwork_url ? [{ src: now.artwork_url, sizes: "512x512" }] : [],
    });
  } else {
    ms.metadata = null;
  }
}

// Keep the OS scrubber position current. Cheap (no allocation), so it runs every
// progress tick. setPositionState throws on inconsistent values (e.g. position past
// duration), so guard and clear when unknown.
function updateMediaSessionPosition(playback) {
  if (!("mediaSession" in navigator)) return;
  const ms = navigator.mediaSession;
  try {
    const duration = playback.duration_seconds ?? 0;
    const position = playback.elapsed_seconds ?? 0;
    if (duration > 0 && position >= 0 && position <= duration) {
      ms.setPositionState({ duration, position, playbackRate: 1 });
    } else {
      ms.setPositionState();
    }
  } catch {
    /* ignore invalid position state */
  }
}

// Wire OS media controls (play/pause/prev/next/stop/seek) to the active player.
// Registered once; each handler targets whichever player is currently active.
function setupMediaSession() {
  if (!("mediaSession" in navigator)) return;
  const ms = navigator.mediaSession;
  const send = (body) => {
    if (state.activePlayerId) commandTarget(body);
  };
  const handlers = {
    play: () => {
      if (!state.activePlayerId) return;
      if (browserOutputsFor(state.activePlayerId)) claimBrowserOutput();
      if (state.activeNowTrackId) send({ command: "play" });
      else playTrack(0);
    },
    pause: () => send({ command: "pause" }),
    previoustrack: () => send({ command: "previous" }),
    nexttrack: () => send({ command: "next" }),
    stop: () => send({ command: "stop" }),
    seekto: (details) => {
      if (details.seekTime != null) {
        send({ command: "seek", position_seconds: details.seekTime });
      }
    },
    seekbackward: (details) => {
      const offset = details.seekOffset || 10;
      send({ command: "seek", position_seconds: Math.max(0, state.activeElapsed - offset) });
    },
    seekforward: (details) => {
      const offset = details.seekOffset || 10;
      send({ command: "seek", position_seconds: state.activeElapsed + offset });
    },
  };
  for (const [action, handler] of Object.entries(handlers)) {
    try {
      ms.setActionHandler(action, handler);
    } catch {
      /* browser doesn't support this action */
    }
  }
}

function renderNowArt(now) {
  const existing = els.nowArt.querySelector("img");
  if (now && now.artwork_url) {
    if (existing) {
      if (!existing.src.endsWith(now.artwork_url)) existing.src = now.artwork_url;
    } else {
      const img = document.createElement("img");
      img.alt = "";
      img.src = now.artwork_url;
      els.nowArt.prepend(img);
    }
    els.nowArtFallback.hidden = true;
  } else {
    if (existing) existing.remove();
    els.nowArtFallback.hidden = false;
    els.nowArtFallback.textContent = now?.title ? now.title.trim().charAt(0).toUpperCase() : "♪";
  }
}

// Smoothly advance the elapsed readout between server updates while playing.
setInterval(() => {
  if (state.activeStatus !== "playing" || state.seekDragging) return;
  if (state.activeDuration > 0 && state.activeElapsed >= state.activeDuration) return;
  state.activeElapsed += 1;
  setRange(els.seek, state.activeElapsed, state.activeDuration);
  els.elapsed.textContent = formatTime(state.activeElapsed);
}, 1000);

loadPlayers();
setupMediaSession();
