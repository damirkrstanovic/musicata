<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, type DirectoryStation } from "../lib/api";
  import { radio } from "../lib/radio.svelte";
  import { player } from "../lib/player.svelte";
  import { playStream } from "../lib/playback";
  import { confirmAction } from "../lib/modal";
  import type { RadioStation } from "../types/RadioStation";

  let query = $state("");
  let directory = $state<DirectoryStation[]>([]);
  let loading = $state(true);
  let directoryError = $state("");
  let savedError = $state("");
  let actionError = $state("");
  let busy = $state(false);
  let name = $state("");
  let streamUrl = $state("");
  let request = 0;
  const message = (error: unknown) => error instanceof Error ? error.message : String(error);

  async function loadSaved() {
    savedError = "";
    try { await radio.load(); } catch (error) { savedError = message(error); }
  }

  async function browse() {
    const current = ++request;
    loading = true;
    directoryError = "";
    try {
      const stations = await api.radioDirectory(query.trim());
      if (current === request) directory = stations;
    } catch (error) {
      if (current === request) { directory = []; directoryError = message(error); }
    } finally { if (current === request) loading = false; }
  }
  void loadSaved();
  void browse();

  async function save(station: {name: string; stream_url: string; homepage_url?: string | null}, play = false) {
    const targetEpoch = player.targetEpoch;
    busy = true;
    actionError = "";
    try {
      const saved = await radio.save(station);
      if (play && targetEpoch === player.targetEpoch) await playSaved(saved);
      return true;
    } catch (error) { actionError = message(error); return false; }
    finally { busy = false; }
  }

  async function playSaved(station: RadioStation) {
    actionError = "";
    try {
      if (!await playStream(`/api/radio/${encodeURIComponent(station.id)}/stream`, station.name)) {
        actionError = "Couldn't start playback. Check the selected output and try again.";
      }
    }
    catch (error) { actionError = message(error); }
  }

  async function add(event: SubmitEvent) {
    event.preventDefault();
    if (await save({name: name.trim(), stream_url: streamUrl.trim()})) { name = ""; streamUrl = ""; }
  }

  async function remove(station: RadioStation) {
    if (!await confirmAction({title: "Remove station", message: `Remove ${station.name} from your saved stations?`, confirmLabel: "Remove"})) return;
    busy = true;
    actionError = "";
    try { await radio.remove(station.id); } catch (error) { actionError = message(error); }
    finally { busy = false; }
  }
</script>

<div class="radio-view">
  <section class="radio-saved" aria-label="Saved radio stations">
    <h3>Saved stations</h3>
    {#if savedError}<p role="alert">Couldn't load saved stations: {savedError} <button class="ghost-button" type="button" onclick={loadSaved}>Retry</button></p>{/if}
    {#if !radio.stations.length && !savedError}<p class="admin-hint">No saved stations yet. Discover one below or add a stream URL.</p>{/if}
    {#each radio.stations as station (station.id)}
      <div class="station-row saved-station">
        <strong>{station.name}</strong>
        <div class="station-actions">
          <button class="ghost-button" type="button" onclick={() => playSaved(station)}>Play</button>
          <button class="ghost-button danger" type="button" disabled={busy} onclick={() => remove(station)} aria-label={`Remove ${station.name}`}>Remove</button>
        </div>
      </div>
    {/each}
  </section>

  <section aria-label="Add a radio station">
    <h3>Add a station</h3>
    <form class="radio-add" onsubmit={add}>
      <label class="field"><span>Name</span><input name="name" required bind:value={name} placeholder="My station" /></label>
      <label class="field stream-field"><span>Stream URL</span><input name="stream_url" type="url" pattern="https?://.*" required bind:value={streamUrl} placeholder="https://example.com/live.mp3" /></label>
      <button class="primary-button" type="submit" disabled={busy}>Add station</button>
    </form>
    {#if actionError}<p role="alert">{actionError}</p>{/if}
  </section>

  <section class="radio-directory" aria-label="Discover radio stations">
    <h3>Discover stations</h3>
    <p class="admin-hint">Popular stations from Radio Browser. Search by station name, or save a station to keep it here.</p>
    <form class="radio-search" onsubmit={event => {event.preventDefault(); void browse();}}>
      <label class="field"><span>Station name</span><input type="search" aria-label="Search radio stations" bind:value={query} placeholder="Search radio stations" /></label>
      <button class="ghost-button" type="submit">Search</button>
    </form>
    {#if loading}<p class="admin-hint" role="status">Loading stations…</p>
    {:else if directoryError}<p role="alert">Couldn't load the radio directory: {directoryError} <button class="ghost-button" type="button" onclick={browse}>Retry</button></p>
    {:else if !directory.length}<p class="admin-hint">No stations found. Try another name or add a stream URL above.</p>
    {:else}
      {#each directory as station, index (`${station.stream_url}:${index}`)}
        {@const saved = radio.stations.some(s => s.stream_url === station.stream_url)}
        <div class="station-row directory-station">
          <div><strong>{station.name}</strong><p class="admin-hint">{[station.country, station.tags, station.codec, station.bitrate ? `${station.bitrate} kbps` : null].filter(Boolean).join(" · ")}</p></div>
          <div class="station-actions">
            <button class="ghost-button" type="button" disabled={busy} onclick={() => save(station, true)}>Play</button>
            <button class="ghost-button" type="button" disabled={busy || saved} onclick={() => save(station)}>{saved ? "Saved" : "Save"}</button>
          </div>
        </div>
      {/each}
    {/if}
  </section>
</div>

<style>
  .radio-view { display: grid; gap: 1.5rem; padding: 1rem; }
  h3 { margin: 0 0 .75rem; }
  form { display: flex; flex-wrap: wrap; align-items: end; gap: .75rem; }
  .field { flex: 1 1 18ch; max-width: 28ch; min-width: 0; }
  .stream-field { flex-basis: 28ch; max-width: 42ch; }
  input { width: 100%; min-width: 0; }
  .station-row { display: flex; align-items: center; justify-content: space-between; gap: 1rem; padding: .75rem 0; border-bottom: 1px solid var(--line); }
  .station-row > div:first-child { min-width: 0; overflow-wrap: anywhere; }
  .station-actions { display: flex; flex-shrink: 0; gap: .5rem; }
  .admin-hint { margin: .35rem 0; }
  [role="alert"] { color: var(--danger); overflow-wrap: anywhere; }
  @media (max-width: 480px) { .station-row { align-items: start; flex-direction: column; gap: .5rem; } }
</style>
