// Musicata admin page: manage music sources, players/zones, and watch background
// activity (scans, rescans) with clear error reporting. Standalone from the player
// app — it only talks to the JSON API.

const $ = (sel) => document.querySelector(sel);

function escapeHtml(value) {
  return String(value ?? "").replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
  );
}

async function api(path) {
  const response = await fetch(path);
  if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
  return response.json();
}

// --- In-product dialogs (never window.confirm/prompt — suppressed in PWAs/mobile) ---
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
    dialog.addEventListener("submit", (event) => {
      event.preventDefault();
      const out = {};
      for (const [key, input] of inputs) out[key] = input.value.trim();
      close(fields.length ? out : {});
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

async function confirmAction({ title, message, confirmLabel = "Delete", danger = true }) {
  return (await openModal({ title, message, confirmLabel, danger })) !== null;
}

async function promptText({ title, label, value = "", placeholder, confirmLabel = "Save" }) {
  const result = await openModal({
    title,
    fields: [{ key: "value", label, value, placeholder }],
    confirmLabel,
  });
  return result === null ? null : result.value || null;
}

async function apiSend(path, method, body) {
  const init = { method };
  if (body !== undefined) {
    init.headers = { "Content-Type": "application/json" };
    init.body = JSON.stringify(body);
  }
  const response = await fetch(path, init);
  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    try {
      const payload = await response.json();
      if (payload?.error?.message) detail = payload.error.message;
    } catch {
      /* keep status line */
    }
    throw new Error(detail);
  }
  return response.status === 204 ? null : response.json().catch(() => null);
}

function setStatus(el, message, isError) {
  el.textContent = message;
  el.classList.toggle("error", !!isError);
}

// ---- Music sources --------------------------------------------------------

const CAP_LABELS = [
  ["can_scan", "Scan"],
  ["can_browse", "Browse"],
  ["can_search", "Search"],
  ["can_stream", "Stream"],
];

async function loadSources() {
  const list = $("#sources-list");
  let sources;
  try {
    sources = await api("/api/sources");
  } catch (error) {
    list.innerHTML = `<p class="error">Sources unavailable: ${escapeHtml(error.message)}</p>`;
    return;
  }
  list.innerHTML = "";
  for (const source of sources) {
    const row = document.createElement("div");
    row.className = "admin-row";
    const chips = CAP_LABELS.filter(([k]) => source.capabilities?.[k])
      .map(([, label]) => `<span class="chip">${label}</span>`)
      .join("");
    row.innerHTML =
      `<div class="admin-row-main">` +
      `<div class="admin-row-title"><strong>${escapeHtml(source.display_name)}</strong>` +
      `<span class="tag">${escapeHtml(source.kind)}</span></div>` +
      `<div class="chips">${chips}</div></div>`;
    if (source.kind !== "local") {
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "ghost-button danger";
      remove.textContent = "Remove";
      remove.addEventListener("click", () => removeSource(source.id, source.display_name));
      row.append(remove);
    }
    list.append(row);
  }
}

async function removeSource(id, name) {
  const ok = await confirmAction({
    title: "Remove source",
    message: `Remove “${name}”? Its tracks leave the library.`,
    confirmLabel: "Remove",
  });
  if (!ok) return;
  try {
    await apiSend(`/api/sources/${encodeURIComponent(id)}`, "DELETE");
  } catch (error) {
    setStatus($("#add-source-status"), `Could not remove: ${error.message}`, true);
    return;
  }
  await Promise.all([loadSources(), loadActivity()]);
}

$("#add-source").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.currentTarget; // capture before any await (currentTarget is cleared after)
  const body = {
    kind: "smb",
    host: $("#source-host").value.trim(),
    share: $("#source-share").value.trim(),
    base_path: $("#source-path").value.trim() || undefined,
    display_name: $("#source-name").value.trim() || undefined,
    username: $("#source-user").value.trim() || undefined,
    password: $("#source-pass").value || undefined,
  };
  const submit = form.querySelector("button[type=submit]");
  submit.disabled = true;
  setStatus($("#add-source-status"), "Adding…");
  try {
    const added = await apiSend("/api/sources", "POST", body);
    form.reset();
    setStatus(
      $("#add-source-status"),
      `Added “${added?.display_name ?? "source"}” — scanning in the background (see Activity).`,
    );
  } catch (error) {
    setStatus($("#add-source-status"), `Could not add share: ${error.message}`, true);
  } finally {
    submit.disabled = false;
  }
  await Promise.all([loadSources(), loadActivity()]);
});

$("#rescan-all").addEventListener("click", async () => {
  try {
    await apiSend("/api/sources/all/rescan", "POST");
  } catch (error) {
    setStatus($("#add-source-status"), `Rescan failed to start: ${error.message}`, true);
    return;
  }
  await loadActivity();
});

// ---- Players & zones ------------------------------------------------------

let zones = [];

function zoneOptions(selected) {
  const none = `<option value=""${selected ? "" : " selected"}>No zone</option>`;
  return (
    none +
    zones
      .map(
        (z) =>
          `<option value="${escapeHtml(z.id)}"${z.id === selected ? " selected" : ""}>${escapeHtml(z.name)}</option>`,
      )
      .join("")
  );
}

async function loadPlayers() {
  let players;
  try {
    [players, zones] = await Promise.all([api("/api/players"), api("/api/zones")]);
  } catch (error) {
    $("#players-list").innerHTML = `<p class="error">Players unavailable: ${escapeHtml(error.message)}</p>`;
    return;
  }
  renderPlayers(players);
  renderZones();
}

function renderPlayers(players) {
  const list = $("#players-list");
  if (!players.length) {
    list.innerHTML = `<p class="empty">No players yet.</p>`;
    return;
  }
  list.innerHTML = "";
  for (const player of players) {
    const row = document.createElement("div");
    row.className = "admin-row";
    row.innerHTML =
      `<div class="admin-row-main"><div class="admin-row-title">` +
      `<span class="dot ${player.online ? "online" : "offline"}"></span>` +
      `<strong>${escapeHtml(player.name)}</strong><span class="tag">${escapeHtml(player.kind)}</span>` +
      `</div></div>`;

    const zoneSel = document.createElement("select");
    zoneSel.className = "zone-select";
    zoneSel.innerHTML = zoneOptions(player.zone_id);
    zoneSel.addEventListener("change", () =>
      apiSend(`/api/players/${encodeURIComponent(player.id)}`, "PATCH", {
        zone_id: zoneSel.value || null,
      }).then(loadPlayers),
    );
    row.append(zoneSel);

    if (player.kind !== "browser") {
      const rename = document.createElement("button");
      rename.type = "button";
      rename.className = "ghost-button";
      rename.textContent = "Rename";
      rename.addEventListener("click", () => renamePlayer(player));
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "ghost-button danger";
      remove.textContent = "Remove";
      remove.addEventListener("click", () => removePlayer(player));
      row.append(rename, remove);
    }
    list.append(row);
  }
}

async function renamePlayer(player) {
  const name = await promptText({
    title: "Rename player",
    label: "Player name",
    value: player.name,
    confirmLabel: "Rename",
  });
  if (!name) return;
  await apiSend(`/api/players/${encodeURIComponent(player.id)}`, "PATCH", { name });
  loadPlayers();
}

async function removePlayer(player) {
  const ok = await confirmAction({
    title: "Remove player",
    message: `Remove the player “${player.name}”?`,
    confirmLabel: "Remove",
  });
  if (!ok) return;
  await apiSend(`/api/players/${encodeURIComponent(player.id)}`, "DELETE");
  loadPlayers();
}

function renderZones() {
  const list = $("#zones-list");
  if (!zones.length) {
    list.innerHTML = `<p class="empty">No zones.</p>`;
    return;
  }
  list.innerHTML = "";
  for (const zone of zones) {
    const row = document.createElement("div");
    row.className = "admin-row";
    row.innerHTML = `<div class="admin-row-main"><strong>${escapeHtml(zone.name)}</strong></div>`;
    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "ghost-button danger";
    remove.textContent = "Delete";
    remove.addEventListener("click", async () => {
      const ok = await confirmAction({
        title: "Delete zone",
        message: `Delete the zone “${zone.name}”?`,
      });
      if (!ok) return;
      await apiSend(`/api/zones/${encodeURIComponent(zone.id)}`, "DELETE");
      loadPlayers();
    });
    row.append(remove);
    list.append(row);
  }
}

$("#add-player").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.currentTarget;
  const submit = form.querySelector("button[type=submit]");
  submit.disabled = true;
  setStatus($("#add-player-status"), "Connecting…");
  try {
    await apiSend("/api/players", "POST", {
      kind: "mpd",
      address: $("#player-address").value.trim(),
      name: $("#player-name").value.trim() || undefined,
    });
    form.reset();
    setStatus($("#add-player-status"), "");
  } catch (error) {
    setStatus($("#add-player-status"), `Could not add player: ${error.message}`, true);
  } finally {
    submit.disabled = false;
  }
  loadPlayers();
});

$("#add-zone").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.currentTarget;
  try {
    await apiSend("/api/zones", "POST", { name: $("#zone-name").value.trim() });
    form.reset();
  } catch {
    /* ignore */
  }
  loadPlayers();
});

// ---- Activity -------------------------------------------------------------

function timeAgo(unix) {
  if (!unix) return "";
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - unix);
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  return `${Math.floor(secs / 3600)}h ago`;
}

async function loadActivity() {
  try {
    renderActivity(await api("/api/activity"));
  } catch {
    /* the WebSocket will deliver updates; ignore a one-off fetch failure */
  }
}

function renderActivity(items) {
  const running = items.some((a) => a.status === "running");
  $("#activity-live").hidden = !running;

  const list = $("#activity-list");
  if (!items.length) {
    list.innerHTML = `<p class="empty">No recent activity.</p>`;
    return;
  }
  list.innerHTML = items
    .map((a) => {
      const when =
        a.status === "running"
          ? "running…"
          : timeAgo(a.finished_at_unix_seconds ?? a.started_at_unix_seconds);
      const message = a.message
        ? `<pre class="activity-message">${escapeHtml(a.message)}</pre>`
        : "";
      const icon =
        { running: "⟳", ok: "✓", interrupted: "⚠" }[a.status] ?? "✕";
      return (
        `<div class="activity-item status-${a.status}">` +
        `<div class="activity-head">` +
        `<span class="activity-status">${icon}</span>` +
        `<span class="activity-label">${escapeHtml(a.label)}</span>` +
        `<span class="activity-when">${when}</span>` +
        `</div>${message}</div>`
      );
    })
    .join("");
}

// Live activity over a WebSocket — the server pushes the list on every change
// (scan progress, completions, errors), so no polling. Reconnects if it drops.
function connectActivity() {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  let socket;
  try {
    socket = new WebSocket(`${scheme}://${location.host}/api/activity/ws`);
  } catch {
    setTimeout(connectActivity, 3000);
    return;
  }
  socket.onmessage = (event) => {
    try {
      renderActivity(JSON.parse(event.data));
    } catch {
      /* ignore malformed frame */
    }
  };
  socket.onclose = () => setTimeout(connectActivity, 3000);
  socket.onerror = () => {
    try {
      socket.close();
    } catch {
      /* already closing */
    }
  };
}

// ---- Artwork settings -----------------------------------------------------

async function loadArtworkSettings() {
  try {
    const settings = await api("/api/settings");
    $("#artwork-fetch").checked = !!settings.artwork_fetch;
    $("#fingerprint-enabled").checked = !!settings.fingerprint_enabled;
    $("#musicbrainz-enrich-enabled").checked = !!settings.musicbrainz_enrich_enabled;
    $("#fanart-key").value = settings.fanart_tv_key || "";
  } catch {
    /* leave the form at its defaults */
  }
}

$("#artwork-settings").addEventListener("submit", async (event) => {
  event.preventDefault();
  setStatus($("#artwork-settings-status"), "Saving…");
  try {
    await apiSend("/api/settings", "PATCH", {
      artwork_fetch: $("#artwork-fetch").checked,
      fingerprint_enabled: $("#fingerprint-enabled").checked,
      musicbrainz_enrich_enabled: $("#musicbrainz-enrich-enabled").checked,
      fanart_tv_key: $("#fanart-key").value.trim(),
    });
    setStatus($("#artwork-settings-status"), "Saved.");
  } catch (error) {
    setStatus($("#artwork-settings-status"), `Could not save: ${error.message}`, true);
  }
});

// ---- Merged artists -------------------------------------------------------

let allArtists = []; // {id, name} for the merge picker
const mergeSelected = new Map(); // id -> name
let mergeCanonical = null; // id of the artist that survives

async function loadAliases() {
  let groups;
  try {
    groups = await api("/api/artists/aliases");
  } catch (error) {
    $("#aliases-list").innerHTML = `<p class="error">Merges unavailable: ${escapeHtml(error.message)}</p>`;
    return;
  }
  const list = $("#aliases-list");
  list.innerHTML = "";
  if (!groups.length) {
    list.innerHTML = `<p class="admin-hint">No merges yet.</p>`;
    return;
  }
  for (const group of groups) {
    const row = document.createElement("div");
    row.className = "admin-list-item";
    const members = group.members
      .map(
        (key) =>
          `<span class="merge-chip">${escapeHtml(key)}<button type="button" class="merge-unmerge" data-key="${escapeHtml(key)}" title="Unmerge">✕</button></span>`,
      )
      .join("");
    row.innerHTML = `<div><strong>${escapeHtml(group.canonical_name)}</strong> ← ${members}</div>`;
    list.append(row);
  }
  list.querySelectorAll(".merge-unmerge").forEach((button) => {
    button.addEventListener("click", async () => {
      try {
        await apiSend(`/api/artists/aliases/${encodeURIComponent(button.dataset.key)}`, "DELETE");
        await loadAliases();
      } catch (error) {
        setStatus($("#merge-status"), error.message, true);
      }
    });
  });
}

async function loadArtistsForMerge() {
  try {
    const page = await api("/api/artists?limit=10000");
    allArtists = (page.items || []).map((artist) => ({ id: artist.id, name: artist.name }));
  } catch {
    allArtists = [];
  }
}

function renderMergeResults(query) {
  const results = $("#merge-results");
  results.innerHTML = "";
  const needle = query.trim().toLowerCase();
  if (!needle) return;
  const matches = allArtists
    .filter((a) => a.name.toLowerCase().includes(needle) && !mergeSelected.has(a.id))
    .slice(0, 12);
  for (const artist of matches) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "merge-result";
    button.textContent = artist.name;
    button.addEventListener("click", () => {
      mergeSelected.set(artist.id, artist.name);
      if (!mergeCanonical) mergeCanonical = artist.id;
      $("#merge-search").value = "";
      renderMergeResults("");
      renderMergeSelected();
    });
    results.append(button);
  }
}

function renderMergeSelected() {
  const box = $("#merge-selected");
  box.innerHTML = "";
  for (const [id, name] of mergeSelected) {
    const chip = document.createElement("span");
    chip.className = "merge-chip" + (id === mergeCanonical ? " is-canonical" : "");
    chip.innerHTML =
      `<button type="button" class="merge-canon" title="Keep this name">${id === mergeCanonical ? "★" : "☆"}</button>` +
      `${escapeHtml(name)}<button type="button" class="merge-remove" title="Remove">✕</button>`;
    chip.querySelector(".merge-canon").addEventListener("click", () => {
      mergeCanonical = id;
      renderMergeSelected();
    });
    chip.querySelector(".merge-remove").addEventListener("click", () => {
      mergeSelected.delete(id);
      if (mergeCanonical === id) mergeCanonical = mergeSelected.keys().next().value || null;
      renderMergeSelected();
    });
    box.append(chip);
  }
  $("#merge-submit").disabled = mergeSelected.size < 2 || !mergeCanonical;
}

$("#merge-search").addEventListener("input", (event) => renderMergeResults(event.target.value));

$("#merge-artists").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (mergeSelected.size < 2 || !mergeCanonical) return;
  const memberIds = [...mergeSelected.keys()].filter((id) => id !== mergeCanonical);
  try {
    await apiSend("/api/artists/merge", "POST", {
      canonical_id: mergeCanonical,
      member_ids: memberIds,
    });
    setStatus($("#merge-status"), "Merged — the library updates in the background.");
    mergeSelected.clear();
    mergeCanonical = null;
    renderMergeSelected();
    await loadAliases();
  } catch (error) {
    setStatus($("#merge-status"), error.message, true);
  }
});

// ---- Identification (MusicBrainz coverage) --------------------------------

function pct(part, total) {
  return total ? Math.round((part / total) * 100) : 0;
}

async function loadIdentification() {
  try {
    const [stats, albums, artists] = await Promise.all([
      api("/api/identification/stats"),
      api("/api/identification/unidentified?kind=album&limit=25"),
      api("/api/identification/unidentified?kind=artist&limit=25"),
    ]);
    renderIdentStats(stats);
    renderIdentList($("#ident-albums"), albums, (a) => `${a.title} — ${a.artist_name}`);
    renderIdentList($("#ident-artists"), artists, (a) => a.name);
  } catch (error) {
    $("#ident-stats").innerHTML = `<p class="error">Stats unavailable: ${escapeHtml(error.message)}</p>`;
  }
}

function renderIdentStats(stats) {
  const line = (label, group) =>
    `<div class="ident-stat">
       <span class="ident-num">${group.identified.toLocaleString()} / ${group.total.toLocaleString()}</span>
       <span class="ident-label">${label} identified (${pct(group.identified, group.total)}%)</span>
     </div>`;
  const fp = stats.fingerprint;
  $("#ident-stats").innerHTML =
    line("Tracks", stats.tracks) +
    line("Albums", stats.albums) +
    line("Artists", stats.artists) +
    `<div class="ident-stat">
       <span class="ident-num">${fp.resolved.toLocaleString()}</span>
       <span class="ident-label">fingerprint/search matched · ${fp.not_found.toLocaleString()} awaiting retry · ${fp.searched.toLocaleString()} exhausted</span>
     </div>`;
}

function renderIdentList(el, items, label) {
  el.innerHTML = "";
  if (!items.length) {
    el.innerHTML = `<p class="admin-hint">Everything here is identified. 🎉</p>`;
    return;
  }
  for (const item of items) {
    const row = document.createElement("div");
    row.className = "admin-list-item";
    row.innerHTML = `<div>${escapeHtml(label(item))}</div><span class="ident-count">${item.track_count} tracks</span>`;
    el.append(row);
  }
}

// ---- Boot -----------------------------------------------------------------

loadSources();
loadPlayers();
loadActivity();
loadArtworkSettings();
loadAliases();
loadArtistsForMerge();
loadIdentification();
// Coverage grows as the background passes run; refresh periodically.
setInterval(loadIdentification, 30000);
connectActivity();
// Players still poll (for online status), but slowly — the chatty activity feed
// is now push-based.
setInterval(loadPlayers, 10000);
