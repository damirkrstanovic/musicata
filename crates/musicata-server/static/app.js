const state = {
  albums: [],
  visibleAlbums: [],
  browse: { genres: [], years: [], composers: [] },
  browseFilter: { genre: "", year: "", composer: "" },
  tracks: [],
  visibleTracks: [],
  activePlayerId: null,
  activeStatus: "stopped",
  activeNowTrackId: null,
  activeState: null,
  activeElapsed: 0,
  activeDuration: 0,
  seekDragging: false,
  queueOpen: false,
  searchController: null,
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
  audio: document.querySelector("#audio"),
  nowTitle: document.querySelector("#now-title"),
  nowSubtitle: document.querySelector("#now-subtitle"),
  prev: document.querySelector("#prev"),
  next: document.querySelector("#next"),
  playPause: document.querySelector("#play-pause"),
  activePlayer: document.querySelector("#active-player"),
  transport: document.querySelector(".transport"),
  nowArt: document.querySelector("#now-art"),
  nowArtFallback: document.querySelector("#now-art-fallback"),
  seek: document.querySelector("#seek"),
  elapsed: document.querySelector("#elapsed"),
  duration: document.querySelector("#duration"),
  shuffle: document.querySelector("#shuffle"),
  repeat: document.querySelector("#repeat"),
  footerVolume: document.querySelector("#footer-volume"),
  outputSignal: document.querySelector("#output-signal"),
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

async function tracksApi(filter = {}) {
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
  if (filter.folder) {
    params.set("folder", filter.folder);
  }
  // /api/tracks returns a pagination envelope; the UI works with the items array.
  const page = await api(`/api/tracks${params.size ? `?${params}` : ""}`);
  return page.items;
}

function browseApi() {
  return api("/api/browse");
}

async function loadLibrary() {
  try {
    const [summary, albumsPage, tracks, browse] = await Promise.all([
      api("/api/library/summary"),
      api("/api/albums"),
      tracksApi(),
      browseApi(),
    ]);

    const albums = albumsPage.items;
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

// Play the current track list on the active player, starting at `index`.
async function playTrack(index) {
  const tracks = state.visibleTracks;
  if (tracks.length === 0 || !state.activePlayerId) {
    return;
  }
  if (state.activePlayerId === browserPlayerId) {
    claimBrowserOutput();
    // Start the clicked track within this user gesture so the browser's autoplay
    // policy lets it play; the server round-trip then keeps it in sync.
    const track = tracks[index];
    if (track) {
      els.audio.src = track.stream_url;
      els.audio.play().catch(() => {});
    }
  }
  const ids = tracks.map((track) => track.id);
  await playerCommand(state.activePlayerId, { command: "play_tracks", track_ids: ids });
  if (index > 0) {
    await playerCommand(state.activePlayerId, { command: "play_queue_index", index });
  }
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
// Footer transport targets the active player.
els.prev.addEventListener("click", () => {
  if (state.activePlayerId) playerCommand(state.activePlayerId, { command: "previous" });
});
els.next.addEventListener("click", () => {
  if (state.activePlayerId) playerCommand(state.activePlayerId, { command: "next" });
});
els.playPause.addEventListener("click", () => {
  if (!state.activePlayerId) return;
  if (state.activeStatus === "playing") {
    playerCommand(state.activePlayerId, { command: "pause" });
  } else if (state.activeNowTrackId) {
    if (state.activePlayerId === browserPlayerId) claimBrowserOutput();
    playerCommand(state.activePlayerId, { command: "play" });
  } else {
    // Nothing queued yet: start the current track list.
    playTrack(0);
  }
});
els.activePlayer.addEventListener("change", () => setActivePlayer(els.activePlayer.value));

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
    playerCommand(state.activePlayerId, { command: "seek", position_seconds: value });
  }
});

els.shuffle.addEventListener("click", () => {
  if (!state.activePlayerId) return;
  const enabled = !(state.activeState?.shuffle ?? false);
  playerCommand(state.activePlayerId, { command: "set_shuffle", enabled });
});

els.repeat.addEventListener("click", () => {
  if (!state.activePlayerId) return;
  const order = ["off", "all", "one"];
  const current = state.activeState?.repeat ?? "off";
  const mode = order[(order.indexOf(current) + 1) % order.length];
  playerCommand(state.activePlayerId, { command: "set_repeat", mode });
});

let footerVolumeTimer = 0;
els.footerVolume.addEventListener("input", () => {
  const volume = Number.parseInt(els.footerVolume.value, 10);
  els.footerVolume.style.setProperty("--fill", `${volume}%`);
  if (state.activePlayerId === browserPlayerId && browserOutput) {
    els.audio.volume = Math.min(1, Math.max(0, volume / 100));
  }
  clearTimeout(footerVolumeTimer);
  footerVolumeTimer = setTimeout(() => {
    if (state.activePlayerId) {
      playerCommand(state.activePlayerId, { command: "set_volume", volume });
    }
  }, 150);
});

// Queue drawer.
function toggleQueue(open) {
  state.queueOpen = open ?? !state.queueOpen;
  els.queueDrawer.hidden = !state.queueOpen;
  els.queueToggle.setAttribute("aria-pressed", String(state.queueOpen));
  if (state.queueOpen) renderQueue();
}

els.queueToggle.addEventListener("click", () => toggleQueue());
els.queueClose.addEventListener("click", () => toggleQueue(false));
els.queueClear.addEventListener("click", () => {
  if (state.activePlayerId) playerCommand(state.activePlayerId, { command: "clear" });
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
    if (state.activePlayerId === browserPlayerId) claimBrowserOutput();
    playerCommand(state.activePlayerId, { command: "play_queue_index", index });
  } else if (action === "remove") {
    playerCommand(state.activePlayerId, { command: "remove_queue_item", index });
  } else if (action === "up") {
    playerCommand(state.activePlayerId, { command: "move_queue_item", from: index, to: index - 1 });
  } else if (action === "down") {
    playerCommand(state.activePlayerId, { command: "move_queue_item", from: index, to: index + 1 });
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

// ---- Players & zones ------------------------------------------------------

const playerEls = {
  list: document.querySelector("#players-list"),
  addForm: document.querySelector("#add-player"),
  address: document.querySelector("#player-address"),
  name: document.querySelector("#player-name"),
  zonesList: document.querySelector("#zones-list"),
  addZoneForm: document.querySelector("#add-zone"),
  zoneName: document.querySelector("#zone-name"),
  addStatus: document.querySelector("#add-player-status"),
};

const playerSockets = new Map();
let playerData = { players: [], zones: [] };

async function apiJson(path, method, body) {
  const init = { method };
  if (body !== undefined) {
    init.headers = { "Content-Type": "application/json" };
    init.body = JSON.stringify(body);
  }
  const response = await fetch(path, init);
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return response.status === 204 ? null : response.json();
}

async function loadPlayers() {
  if (!playerEls.list) return;
  try {
    const [players, zones] = await Promise.all([api("/api/players"), api("/api/zones")]);
    playerData = { players, zones };
    browserPlayerId = players.find((player) => player.kind === "browser")?.id ?? null;
    renderPlayers();
    populateActivePlayerSelect(players);
  } catch (error) {
    playerEls.list.innerHTML = `<p class="error">Players unavailable: ${escapeHtml(error.message)}</p>`;
  }
}

function zoneOptions(selected) {
  const none = `<option value=""${selected ? "" : " selected"}>No zone</option>`;
  const options = playerData.zones
    .map(
      (zone) =>
        `<option value="${escapeHtml(zone.id)}"${zone.id === selected ? " selected" : ""}>${escapeHtml(zone.name)}</option>`,
    )
    .join("");
  return none + options;
}

function renderPlayers() {
  // Drop sockets for players that no longer exist.
  for (const [id, socket] of playerSockets) {
    if (!playerData.players.some((player) => player.id === id)) {
      socket.close();
      playerSockets.delete(id);
    }
  }

  playerEls.list.innerHTML =
    playerData.players.length === 0
      ? `<p class="empty">No players yet. Add an MPD host:port below.</p>`
      : playerData.players
          .map(
            (player) => `
        <div class="player-card" data-player="${escapeHtml(player.id)}">
          <div class="player-head">
            <span class="player-dot ${player.online ? "online" : "offline"}"></span>
            <strong>${escapeHtml(player.name)}</strong>
            <button class="icon-button" data-action="rename" title="Rename">&#9998;</button>
            <button class="icon-button" data-action="remove" title="Remove">&times;</button>
          </div>
          <p class="player-now" data-now="${escapeHtml(player.id)}">${player.online ? "Idle" : "Offline"}</p>
          <div class="player-controls-row">
            <button data-action="previous" title="Previous">&#9198;</button>
            <button data-action="play" title="Play">&#9654;</button>
            <button data-action="pause" title="Pause">&#9208;</button>
            <button data-action="stop" title="Stop">&#9209;</button>
            <button data-action="next" title="Next">&#9197;</button>
            <button data-action="play-here" class="ghost-button">Play view here</button>
          </div>
          <label class="player-volume" title="Volume">
            <span>&#128266;</span>
            <input type="range" min="0" max="100" value="100" data-action="volume" data-volume="${escapeHtml(player.id)}">
          </label>
          <label class="player-zone">
            <span>Zone</span>
            <select data-action="zone">${zoneOptions(player.zone_id)}</select>
          </label>
        </div>`,
          )
          .join("");

  playerEls.zonesList.innerHTML =
    playerData.zones.length === 0
      ? `<p class="empty">No zones.</p>`
      : playerData.zones
          .map(
            (zone) => `
        <div class="zone-row" data-zone="${escapeHtml(zone.id)}">
          <span>${escapeHtml(zone.name)}</span>
          <button class="icon-button" data-action="delete-zone" title="Delete zone">&times;</button>
        </div>`,
          )
          .join("");

  for (const player of playerData.players) {
    if (!playerSockets.has(player.id)) {
      openPlayerSocket(player.id);
    }
  }
}

function openPlayerSocket(id) {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  let socket;
  try {
    socket = new WebSocket(`${scheme}://${location.host}/api/players/${encodeURIComponent(id)}/ws`);
  } catch {
    return;
  }
  socket.onmessage = (event) => {
    try {
      const playback = JSON.parse(event.data);
      const node = playerEls.list.querySelector(`[data-now="${cssEscape(id)}"]`);
      if (node) {
        const suffix = id === browserPlayerId && browserOutput ? " (playing here)" : "";
        node.textContent = nowPlayingText(playback) + suffix;
      }
      const volume = playerEls.list.querySelector(`input[data-volume="${cssEscape(id)}"]`);
      if (volume && playback.volume != null && document.activeElement !== volume) {
        volume.value = String(playback.volume);
      }
      if (
        id === browserPlayerId &&
        browserOutput &&
        state.activePlayerId === browserPlayerId
      ) {
        driveBrowserAudio(playback);
      }
      if (id === state.activePlayerId) {
        updateFooterFromState(playback);
      }
    } catch {
      /* ignore malformed frames */
    }
  };
  socket.onclose = () => playerSockets.delete(id);
  playerSockets.set(id, socket);
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

function browserSocketSend(message) {
  const socket = browserPlayerId ? playerSockets.get(browserPlayerId) : null;
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

async function playerCommand(id, command) {
  try {
    await apiJson(`/api/players/${encodeURIComponent(id)}/commands`, "POST", command);
  } catch (error) {
    console.error("player command failed", error);
  }
}

// ---- Active player (footer + main list target) ----------------------------

function savedActivePlayer() {
  try {
    return localStorage.getItem("musicata-active-player");
  } catch {
    return null;
  }
}

function populateActivePlayerSelect(players) {
  if (!els.activePlayer) return;
  els.activePlayer.innerHTML = players
    .map(
      (player) =>
        `<option value="${escapeHtml(player.id)}">${escapeHtml(player.name)}${player.online ? "" : " (offline)"}</option>`,
    )
    .join("");
  const saved = savedActivePlayer();
  const active =
    players.find((player) => player.id === saved)?.id ||
    browserPlayerId ||
    players[0]?.id ||
    null;
  if (active) {
    setActivePlayer(active);
  }
}

function setActivePlayer(id) {
  if (!id) return;
  state.activePlayerId = id;
  if (els.activePlayer && els.activePlayer.value !== id) {
    els.activePlayer.value = id;
  }
  try {
    localStorage.setItem("musicata-active-player", id);
  } catch {
    /* ignore */
  }
  if (id !== browserPlayerId) {
    // A remote player renders its own audio; this tab must stay silent.
    els.audio.pause();
  }
  refreshActivePlayerFooter();
}

async function refreshActivePlayerFooter() {
  if (!state.activePlayerId) return;
  try {
    const playback = await api(
      `/api/players/${encodeURIComponent(state.activePlayerId)}/state`,
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

function updateFooterFromState(playback) {
  state.activeState = playback;
  state.activeStatus = playback.status;
  const now = playback.now_playing;
  state.activeNowTrackId = now?.track_id ?? null;
  state.activeElapsed = playback.elapsed_seconds ?? 0;
  state.activeDuration = playback.duration_seconds ?? 0;

  els.transport.dataset.status = playback.status;

  // Now-playing art (real cover when known, monogram fallback otherwise).
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

  els.playPause.textContent = playback.status === "playing" ? "❚❚" : "▶";
  els.playPause.title = playback.status === "playing" ? "Pause" : "Play";

  // Shuffle + repeat toggles.
  els.shuffle.classList.toggle("active", Boolean(playback.shuffle));
  els.shuffle.setAttribute("aria-pressed", String(Boolean(playback.shuffle)));
  els.repeat.dataset.mode = playback.repeat || "off";
  els.repeat.classList.toggle("active", playback.repeat && playback.repeat !== "off");
  els.repeat.innerHTML = playback.repeat === "one" ? "↻<span class='one'>1</span>" : "↻";

  // Volume (unless the user is adjusting it).
  if (playback.volume != null && document.activeElement !== els.footerVolume) {
    setRange(els.footerVolume, playback.volume, 100);
  }

  // Seek (unless dragging).
  if (!state.seekDragging) {
    setRange(els.seek, state.activeElapsed, state.activeDuration);
    els.elapsed.textContent = formatTime(state.activeElapsed);
    els.duration.textContent = formatTime(state.activeDuration);
  }

  updateOutputSignal();
  els.queueCount.textContent = String(playback.queue?.length ?? 0);
  if (state.queueOpen) renderQueue();

  if ("mediaSession" in navigator) {
    navigator.mediaSession.playbackState =
      playback.status === "playing"
        ? "playing"
        : playback.status === "paused"
          ? "paused"
          : "none";
  }
  markActiveTrack();
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

function updateOutputSignal() {
  const here = state.activePlayerId === browserPlayerId && browserOutput;
  const player = playerData.players.find((entry) => entry.id === state.activePlayerId);
  els.outputSignal.classList.toggle("here", here);
  els.outputSignal.classList.toggle("online", !here && Boolean(player?.online));
}

// Smoothly advance the elapsed readout between server updates while playing.
setInterval(() => {
  if (state.activeStatus !== "playing" || state.seekDragging) return;
  if (state.activeDuration > 0 && state.activeElapsed >= state.activeDuration) return;
  state.activeElapsed += 1;
  setRange(els.seek, state.activeElapsed, state.activeDuration);
  els.elapsed.textContent = formatTime(state.activeElapsed);
}, 1000);

if (playerEls.list) {
  playerEls.list.addEventListener("click", async (event) => {
    const button = event.target.closest("button[data-action]");
    if (!button) return;
    const id = button.closest("[data-player]")?.dataset.player;
    if (!id) return;
    const action = button.dataset.action;
    // Interacting with the browser player makes this tab its audio output.
    if (
      id === browserPlayerId &&
      ["play", "play-here", "next", "previous"].includes(action)
    ) {
      claimBrowserOutput();
    }
    if (["play", "pause", "stop", "next", "previous"].includes(action)) {
      await playerCommand(id, { command: action });
    } else if (action === "play-here") {
      const ids = state.visibleTracks.map((track) => track.id);
      if (ids.length) {
        await playerCommand(id, { command: "play_tracks", track_ids: ids });
      }
    } else if (action === "rename") {
      const current = playerData.players.find((player) => player.id === id);
      const name = prompt("Player name", current?.name ?? "");
      if (name) {
        await apiJson(`/api/players/${encodeURIComponent(id)}`, "PATCH", { name });
        await loadPlayers();
      }
    } else if (action === "remove") {
      if (confirm("Remove this player?")) {
        await apiJson(`/api/players/${encodeURIComponent(id)}`, "DELETE");
        await loadPlayers();
      }
    }
  });

  playerEls.list.addEventListener("change", async (event) => {
    const select = event.target.closest("select[data-action='zone']");
    if (!select) return;
    const id = select.closest("[data-player]")?.dataset.player;
    if (!id) return;
    await apiJson(`/api/players/${encodeURIComponent(id)}`, "PATCH", {
      zone_id: select.value || null,
    });
    await loadPlayers();
  });

  // Volume slider: instant local feedback for the browser output, debounced
  // command to the player so dragging doesn't flood it.
  const volumeTimers = new Map();
  playerEls.list.addEventListener("input", (event) => {
    const slider = event.target.closest("input[data-action='volume']");
    if (!slider) return;
    const id = slider.closest("[data-player]")?.dataset.player;
    if (!id) return;
    const volume = Number.parseInt(slider.value, 10);
    if (id === browserPlayerId && browserOutput) {
      els.audio.volume = Math.min(1, Math.max(0, volume / 100));
    }
    clearTimeout(volumeTimers.get(id));
    volumeTimers.set(
      id,
      setTimeout(() => playerCommand(id, { command: "set_volume", volume }), 150),
    );
  });
}

if (playerEls.addForm) {
  playerEls.addForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const address = playerEls.address.value.trim();
    if (!address) return;
    const name = playerEls.name.value.trim();
    playerEls.addStatus.classList.remove("error");
    playerEls.addStatus.textContent = `Registering ${address}…`;
    try {
      const player = await apiJson("/api/players", "POST", { address, name: name || undefined });
      playerEls.address.value = "";
      playerEls.name.value = "";
      await loadPlayers();
      // Connection happens in the background; the dot turns green once reached.
      playerEls.addStatus.textContent = `Added ${player.name}. Connecting…`;
      setTimeout(() => loadPlayers(), 1500);
      setTimeout(() => {
        if (playerEls.addStatus.textContent.startsWith("Added")) {
          playerEls.addStatus.textContent = "";
        }
      }, 4000);
    } catch (error) {
      playerEls.addStatus.classList.add("error");
      playerEls.addStatus.textContent = `Could not add player: ${error.message}`;
    }
  });
}

if (playerEls.zonesList) {
  playerEls.zonesList.addEventListener("click", async (event) => {
    const button = event.target.closest("button[data-action='delete-zone']");
    if (!button) return;
    const id = button.closest("[data-zone]")?.dataset.zone;
    if (id && confirm("Delete this zone?")) {
      await apiJson(`/api/zones/${encodeURIComponent(id)}`, "DELETE");
      await loadPlayers();
    }
  });
}

if (playerEls.addZoneForm) {
  playerEls.addZoneForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const name = playerEls.zoneName.value.trim();
    if (!name) return;
    try {
      await apiJson("/api/zones", "POST", { name });
      playerEls.zoneName.value = "";
      await loadPlayers();
    } catch (error) {
      alert(`Add zone failed: ${error.message}`);
    }
  });
}

loadPlayers();
