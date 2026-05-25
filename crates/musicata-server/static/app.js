const SERVER_CHECK_INTERVAL_MS = 3000;
const SERVER_CHECK_TIMEOUT_MS = 1500;
const SERVER_EVENT_TIMEOUT_MS = 3500;

const state = {
  albums: [],
  visibleAlbums: [],
  browse: { genres: [], years: [], composers: [] },
  browseFilter: { genre: "", year: "", composer: "" },
  tracks: [],
  visibleTracks: [],
  currentIndex: -1,
  searchController: null,
  serverCheckTimer: 0,
  serverCheckPending: false,
  playbackSession: null,
  playbackEvents: null,
  playbackGeneration: 0,
  playbackWatchdogTimer: 0,
  lastServerEventAt: 0,
  metadataTrackId: null,
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
  summary: document.querySelector("#summary"),
  search: document.querySelector("#search"),
  browseGenre: document.querySelector("#browse-genre"),
  browseYear: document.querySelector("#browse-year"),
  browseComposer: document.querySelector("#browse-composer"),
  browseClear: document.querySelector("#browse-clear"),
  albums: document.querySelector("#albums"),
  trackList: document.querySelector("#track-list"),
  viewTitle: document.querySelector("#view-title"),
  refresh: document.querySelector("#refresh"),
  player: document.querySelector(".player"),
  audio: document.querySelector("#audio"),
  nowArt: document.querySelector("#now-art"),
  nowTitle: document.querySelector("#now-title"),
  nowSubtitle: document.querySelector("#now-subtitle"),
  prev: document.querySelector("#prev"),
  next: document.querySelector("#next"),
  playPause: document.querySelector("#play-pause"),
  metadataPanel: document.querySelector("#metadata-panel"),
  metadataTitle: document.querySelector("#metadata-title"),
  metadataBody: document.querySelector("#metadata-body"),
  metadataClose: document.querySelector("#metadata-close"),
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

async function searchApi(query, signal) {
  const response = await fetch(`/api/search?q=${encodeURIComponent(query)}`, { signal });
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.json();
}

function tracksApi(filter = {}) {
  const params = new URLSearchParams();
  if (filter.genre) {
    params.set("genre", filter.genre);
  }
  if (filter.year) {
    params.set("year", filter.year);
  }
  if (filter.composer) {
    params.set("composer", filter.composer);
  }
  return api(`/api/tracks${params.size ? `?${params}` : ""}`);
}

function browseApi() {
  return api("/api/browse");
}

async function loadLibrary() {
  try {
    const [summary, albums, tracks, browse] = await Promise.all([
      api("/api/library/summary"),
      api("/api/albums"),
      tracksApi(),
      browseApi(),
    ]);

    state.albums = albums;
    state.tracks = tracks;
    state.browse = browse;
    els.summary.textContent = `${summary.track_count} tracks, ${summary.album_count} albums`;
    renderBrowseFilters();

    if (hasBrowseFilter()) {
      await applyBrowseFilter({ clearSearch: false });
    } else {
      state.visibleTracks = tracks;
      els.viewTitle.textContent = "Tracks";
      renderAlbums(albums);
      renderTracks(tracks);
    }

    if (state.metadataTrackId && !tracks.some((track) => track.id === state.metadataTrackId)) {
      closeMetadata();
    } else {
      renderMetadataPanel();
    }
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
  state.visibleAlbums = albums;
  els.albums.innerHTML = "";

  for (const album of albums) {
    const button = document.createElement("button");
    button.className = "album";
    button.type = "button";
    button.dataset.albumId = album.id;
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

  if (!hasBrowseFilter()) {
    state.visibleTracks = state.tracks;
    els.viewTitle.textContent = "Tracks";
    renderAlbums(state.albums);
    renderTracks(state.tracks);
    return;
  }

  try {
    const tracks = await tracksApi(state.browseFilter);
    state.visibleTracks = tracks;
    els.viewTitle.textContent = browseTitle(state.browseFilter, tracks.length);
    renderAlbums(albumsForTracks(tracks));
    renderTracks(tracks);
  } catch (error) {
    els.trackList.innerHTML = `<p class="error">Browse failed: ${escapeHtml(error.message)}</p>`;
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

function browseTitle(filter, trackCount) {
  const parts = cleanParts([filter.genre, filter.year, filter.composer]);
  return `Browse: ${parts.join(" / ")} (${trackCount} tracks)`;
}

function renderTracks(tracks) {
  els.trackList.innerHTML = "";

  if (tracks.length === 0) {
    els.trackList.innerHTML = `<p class="empty">No tracks found.</p>`;
    return;
  }

  for (const [index, track] of tracks.entries()) {
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

    const metadataButton = document.createElement("button");
    metadataButton.className = "track-action";
    metadataButton.type = "button";
    metadataButton.textContent = "Metadata";
    metadataButton.addEventListener("click", () => openMetadata(track.id));

    row.append(playButton, metadataButton);
    els.trackList.append(row);
  }

  markActiveTrack();
  markMetadataTrack();
}

async function playTrack(index) {
  const track = state.visibleTracks[index];
  if (!track) {
    return;
  }

  let session;
  try {
    session = await startPlaybackSession();
  } catch (error) {
    stopPlayback(`Unable to start playback: ${error.message}`);
    return;
  }

  if (!session) {
    return;
  }

  state.currentIndex = index;
  els.audio.src = streamUrlForSession(track.stream_url, session.id);
  els.audio.play().catch(() => {});
  updateNowPlaying(track);
  markActiveTrack();
}

function stopPlayback(message) {
  closePlaybackSession();
  stopPlaybackServerMonitor();
  stopPlaybackEventWatchdog();
  els.audio.pause();
  els.audio.removeAttribute("src");
  els.audio.load();
  state.currentIndex = -1;
  els.playPause.textContent = "Play";
  els.nowTitle.textContent = message ? "Playback stopped" : "Nothing playing";
  els.nowSubtitle.textContent = message || "Select a track to start browser playback.";
  els.nowArt.src = "";
  els.nowArt.hidden = true;
  updatePlayerLayout(false, false);
  markActiveTrack();

  if ("mediaSession" in navigator) {
    navigator.mediaSession.metadata = null;
    navigator.mediaSession.playbackState = "none";
  }
}

async function startPlaybackSession() {
  closePlaybackSession();
  const generation = ++state.playbackGeneration;
  const session = await apiPost("/api/playback/sessions");

  if (generation !== state.playbackGeneration) {
    endPlaybackSession(session.id);
    return null;
  }

  state.playbackSession = session;
  state.lastServerEventAt = Date.now();

  if ("EventSource" in window) {
    const events = new EventSource(session.event_url);
    state.playbackEvents = events;
    events.addEventListener("open", recordServerEvent);
    events.addEventListener("heartbeat", recordServerEvent);
    events.addEventListener("message", recordServerEvent);
    events.addEventListener("error", () => {
      if (state.playbackEvents === events && els.audio.src) {
        stopPlayback("Musicata server connection was lost. Playback was stopped.");
      }
    });
    startPlaybackEventWatchdog();
  }

  return session;
}

function closePlaybackSession() {
  const session = state.playbackSession;
  state.playbackSession = null;
  state.playbackGeneration += 1;

  if (state.playbackEvents) {
    state.playbackEvents.close();
    state.playbackEvents = null;
  }

  if (session) {
    endPlaybackSession(session.id);
  }
}

function endPlaybackSession(id) {
  fetch(`/api/playback/sessions/${encodeURIComponent(id)}`, {
    method: "DELETE",
    keepalive: true,
  }).catch(() => {});
}

function streamUrlForSession(streamUrl, sessionId) {
  const separator = streamUrl.includes("?") ? "&" : "?";
  return `${streamUrl}${separator}playback_session=${encodeURIComponent(sessionId)}`;
}

function updateNowPlaying(track) {
  const album = state.albums.find((item) => item.id === track.album_id);
  const artworkUrl = album?.artwork_url || "";
  els.nowTitle.textContent = track.title;
  els.nowSubtitle.textContent = `${track.artist_name} - ${track.album_title}`;
  els.nowArt.src = artworkUrl;
  els.nowArt.hidden = !artworkUrl;
  updatePlayerLayout(Boolean(artworkUrl), true);

  if ("mediaSession" in navigator) {
    navigator.mediaSession.metadata = new MediaMetadata({
      title: track.title,
      artist: track.artist_name,
      album: track.album_title,
      artwork: artworkUrl ? [{ src: artworkUrl }] : [],
    });
  }
}

function updatePlayerLayout(hasArtwork, hasAudio) {
  els.player.classList.toggle("no-art", !hasArtwork);
  els.player.classList.toggle("no-audio", !hasAudio);
  els.audio.hidden = !hasAudio;
}

function startPlaybackServerMonitor() {
  if (state.playbackEvents) {
    return;
  }

  if (state.serverCheckTimer) {
    return;
  }

  checkPlaybackServer();
  state.serverCheckTimer = window.setInterval(
    checkPlaybackServer,
    SERVER_CHECK_INTERVAL_MS,
  );
}

function stopPlaybackServerMonitor() {
  if (!state.serverCheckTimer) {
    return;
  }

  window.clearInterval(state.serverCheckTimer);
  state.serverCheckTimer = 0;
}

async function checkPlaybackServer() {
  if (state.serverCheckPending || els.audio.paused || !els.audio.src) {
    return;
  }

  state.serverCheckPending = true;
  try {
    await fetchWithTimeout("/api/health", SERVER_CHECK_TIMEOUT_MS);
  } catch (_error) {
    stopPlayback("Musicata server is unavailable. Playback was stopped.");
  } finally {
    state.serverCheckPending = false;
  }
}

function startPlaybackEventWatchdog() {
  if (state.playbackWatchdogTimer) {
    return;
  }

  state.playbackWatchdogTimer = window.setInterval(() => {
    if (!state.playbackEvents || !els.audio.src) {
      return;
    }

    if (Date.now() - state.lastServerEventAt > SERVER_EVENT_TIMEOUT_MS) {
      stopPlayback("Musicata server connection was lost. Playback was stopped.");
    }
  }, 1000);
}

function stopPlaybackEventWatchdog() {
  if (!state.playbackWatchdogTimer) {
    return;
  }

  window.clearInterval(state.playbackWatchdogTimer);
  state.playbackWatchdogTimer = 0;
}

function recordServerEvent() {
  state.lastServerEventAt = Date.now();
}

async function fetchWithTimeout(path, timeoutMs) {
  const controller = new AbortController();
  const timeout = window.setTimeout(() => controller.abort(), timeoutMs);

  try {
    const response = await fetch(path, {
      cache: "no-store",
      signal: controller.signal,
    });
    if (!response.ok) {
      throw new Error(`${response.status} ${response.statusText}`);
    }
    return response;
  } finally {
    window.clearTimeout(timeout);
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

async function openMetadata(trackId) {
  state.metadataTrackId = trackId;
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

  const track = metadataTrack();
  if (state.metadataTrackId === trackId && track?.album_id) {
    loadAlbumArtworkReview(track.album_id, trackId);
  }
}

function closeMetadata() {
  state.metadataTrackId = null;
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

  for (const visibleAlbum of state.visibleAlbums) {
    if (visibleAlbum.id === albumId) {
      visibleAlbum.artwork_url = artworkUrl;
    }
  }
  renderAlbums(state.visibleAlbums);

  const currentTrack = state.visibleTracks[state.currentIndex];
  if (currentTrack?.album_id === albumId) {
    updateNowPlaying(currentTrack);
  }
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

function metadataTrack() {
  return state.tracks.find((track) => track.id === state.metadataTrackId);
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
    if (hasBrowseFilter()) {
      await applyBrowseFilter({ clearSearch: false });
      return;
    }
    state.visibleTracks = state.tracks;
    els.viewTitle.textContent = "Tracks";
    renderAlbums(state.albums);
    renderTracks(state.tracks);
    return;
  }

  if (hasBrowseFilter()) {
    clearBrowseFilter();
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

function albumsForTracks(tracks) {
  const albumIds = new Set(tracks.map((track) => track.album_id));
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
els.browseGenre.addEventListener("change", () => applyBrowseFilter());
els.browseYear.addEventListener("change", () => applyBrowseFilter());
els.browseComposer.addEventListener("change", () => applyBrowseFilter());
els.browseClear.addEventListener("click", () => {
  clearBrowseFilter();
  applyBrowseFilter();
});
els.refresh.addEventListener("click", rescanLibrary);
els.metadataClose.addEventListener("click", closeMetadata);
els.prev.addEventListener("click", () => playNext(-1));
els.next.addEventListener("click", () => playNext(1));
els.audio.addEventListener("ended", () => playNext(1));
els.audio.addEventListener("error", () => {
  if (els.audio.src) {
    stopPlayback("The stream is unavailable. Playback was stopped.");
  }
});
els.audio.addEventListener("play", () => {
  els.playPause.textContent = "Pause";
  startPlaybackServerMonitor();
  if ("mediaSession" in navigator) {
    navigator.mediaSession.playbackState = "playing";
  }
});
els.audio.addEventListener("pause", () => {
  els.playPause.textContent = "Play";
  stopPlaybackServerMonitor();
  if ("mediaSession" in navigator) {
    navigator.mediaSession.playbackState = "paused";
  }
});
window.addEventListener("offline", () => {
  if (els.audio.src) {
    stopPlayback("Musicata server is unavailable. Playback was stopped.");
  }
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
