const SERVER_CHECK_INTERVAL_MS = 3000;
const SERVER_CHECK_TIMEOUT_MS = 1500;
const SERVER_EVENT_TIMEOUT_MS = 3500;

const state = {
  albums: [],
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

async function openMetadata(trackId) {
  state.metadataTrackId = trackId;
  state.metadataReview = null;
  state.metadataError = "";
  state.metadataTrackCandidates = null;
  state.metadataAlbumCandidates = null;
  state.metadataCandidateLoading = "";
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
}

function closeMetadata() {
  state.metadataTrackId = null;
  state.metadataReview = null;
  state.metadataError = "";
  state.metadataTrackCandidates = null;
  state.metadataAlbumCandidates = null;
  state.metadataCandidateLoading = "";
  markMetadataTrack();
  renderMetadataPanel();
}

function renderMetadataPanel() {
  const track = metadataTrack();
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
