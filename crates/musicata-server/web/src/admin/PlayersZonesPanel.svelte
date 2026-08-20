<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onDestroy } from "svelte";
  import { api, ApiError, type Player, type Zone } from "../lib/api";
  import { promptText, confirmAction } from "../lib/modal";

  let players = $state<Player[]>([]);
  let zones = $state<Zone[]>([]);
  let status = $state("");
  let error = $state(false);
  let busy = $state(false);

  async function load() {
    try {
      [players, zones] = await Promise.all([api.players(), api.zones()]);
    } catch {
      // leave previous state
    }
  }
  load();
  // Poll for online/offline changes (the old admin did this every 10s).
  const poll = setInterval(load, 10000);
  onDestroy(() => clearInterval(poll));

  function setStatus(message: string, isError = false) {
    status = message;
    error = isError;
  }
  function fail(e: unknown) {
    setStatus(e instanceof ApiError ? e.message : String(e), true);
  }

  async function addPlayer(event: SubmitEvent) {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const data = new FormData(form);
    const address = String(data.get("address") ?? "").trim();
    const name = String(data.get("name") ?? "").trim();
    if (!address) return;
    busy = true;
    setStatus("Connecting…");
    try {
      await api.createPlayer({ kind: "mpd", address, name: name || undefined });
      form.reset();
      setStatus("Added.");
      await load();
    } catch (e) {
      fail(e);
    } finally {
      busy = false;
    }
  }

  async function rename(player: Player) {
    const name = await promptText({ title: "Rename player", label: "Name", value: player.name });
    if (name == null || !name) return;
    try {
      await api.updatePlayer(player.id, { name });
      await load();
    } catch (e) {
      fail(e);
    }
  }

  async function removePlayer(player: Player) {
    if (
      !(await confirmAction({
        title: "Remove player",
        message: `Remove the player “${player.name}”?`,
        confirmLabel: "Remove",
      }))
    )
      return;
    try {
      await api.deletePlayer(player.id);
      await load();
    } catch (e) {
      fail(e);
    }
  }

  async function assignZone(player: Player, zoneId: string) {
    try {
      await api.updatePlayer(player.id, { zone_id: zoneId || null });
      await load();
    } catch (e) {
      fail(e);
    }
  }

  async function addZone(event: SubmitEvent) {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const data = new FormData(form);
    const name = String(data.get("name") ?? "").trim();
    if (!name) return;
    try {
      await api.createZone(name);
      form.reset();
      await load();
    } catch (e) {
      fail(e);
    }
  }

  async function removeZone(zone: Zone) {
    if (!(await confirmAction({ title: `Delete zone ${zone.name}?` }))) return;
    try {
      await api.deleteZone(zone.id);
      await load();
    } catch (e) {
      fail(e);
    }
  }
</script>

<section class="admin-panel">
  <div class="admin-panel-head"><h2>Players &amp; zones</h2></div>
  <p class="admin-hint">MPD players you control, optionally grouped into zones.</p>

  <div class="admin-list">
    {#each players as player (player.id)}
      <div class="admin-row">
        <div class="admin-row-main">
          <div class="admin-row-title">
            <span class="dot" class:online={player.online} class:offline={!player.online}></span>
            <strong>{player.name}</strong>
            <span class="tag">{player.kind}</span>
          </div>
        </div>
        <select
          class="zone-select"
          value={player.zone_id ?? ""}
          onchange={(e) => assignZone(player, (e.currentTarget as HTMLSelectElement).value)}
        >
          <option value="">No zone</option>
          {#each zones as zone (zone.id)}
            <option value={zone.id}>{zone.name}</option>
          {/each}
        </select>
        <button type="button" class="ghost-button" onclick={() => rename(player)}>Rename</button>
        {#if player.kind !== "browser"}
          <button type="button" class="ghost-button danger" onclick={() => removePlayer(player)}>Remove</button>
        {/if}
      </div>
    {/each}
  </div>

  <form class="field-form" onsubmit={addPlayer}>
    <h3>Add an MPD player</h3>
    <div class="field-grid">
      <label class="field"><span>Address</span><input name="address" placeholder="127.0.0.1:6600" required /></label>
      <label class="field"><span>Name (optional)</span><input name="name" placeholder="Living room" /></label>
    </div>
    <div class="field-actions">
      <button type="submit" class="primary-button" disabled={busy}>Add player</button>
      <span class="form-status" class:error>{status}</span>
    </div>
  </form>

  <div class="admin-list">
    {#each zones as zone (zone.id)}
      <div class="admin-row">
        <div class="admin-row-main"><div class="admin-row-title"><strong>{zone.name}</strong></div></div>
        <button type="button" class="ghost-button danger" onclick={() => removeZone(zone)}>Delete</button>
      </div>
    {/each}
  </div>

  <form class="field-form" onsubmit={addZone}>
    <h3>Add a zone</h3>
    <div class="field-grid">
      <label class="field"><span>Name</span><input name="name" placeholder="Upstairs" required /></label>
    </div>
    <div class="field-actions"><button type="submit" class="primary-button">Add zone</button></div>
  </form>
</section>
