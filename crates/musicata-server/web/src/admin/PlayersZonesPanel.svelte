<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onDestroy } from "svelte";
  import { api, ApiError, type Player, type Zone } from "../lib/api";
  import type { PlaybackState } from "../types/PlaybackState";
  import type { SnapClient } from "../types/SnapClient";
  import { promptText, confirmAction } from "../lib/modal";

  let players = $state<Player[]>([]);
  let zones = $state<Zone[]>([]);
  let states = $state<Record<string, PlaybackState | undefined>>({});
  let snapClients = $state<SnapClient[]>([]);
  let snapcastControl = $state("unknown");
  let loadError = $state("");
  let status = $state("");
  let error = $state(false);
  let busy = $state(false);
  let processors = $state<Record<string, {host:string; port:number} | null>>({});
  let editingZoneId = $state<string | null>(null);
  let zoneName = $state("");
  let zoneMembership = $state<Record<string, boolean>>({});

  async function load() {
    try {
      const [nextPlayers, nextZones, snapcast] = await Promise.all([api.players(), api.zones(), api.snapcastStatus().catch(() => null)]);
      players = nextPlayers;
      zones = nextZones;
      snapClients = snapcast?.clients ?? snapcast?.health.clients ?? [];
      snapcastControl = snapcast?.health.control ?? "unavailable";
      loadError = "";
      const stateEntries = await Promise.all([
        ...nextPlayers.map(async player => [player.id, await api.playerState(player.id).catch(() => undefined)] as const),
        ...nextZones.map(async zone => [`zone:${zone.id}`, await api.zoneState(zone.id).catch(() => undefined)] as const),
      ]);
      states = Object.fromEntries(stateEntries);
      for (const player of nextPlayers.filter(player => player.kind === "mpd")) processors[player.id] = await api.dspProcessor(player.id);
    } catch {
      loadError = "Could not refresh players and zones. Existing information may be stale; try again.";
    }
  }
  void load();
  const poll = setInterval(load, 10000);
  onDestroy(() => clearInterval(poll));

  function setStatus(message: string, isError = false) { status = message; error = isError; }
  function fail(e: unknown) { setStatus(e instanceof ApiError ? e.message : "That change could not be saved. Try again.", true); }
  function stateLabel(player: Player) {
    if (!player.online) return "Offline";
    const state = states[player.id];
    return state?.status === "playing" ? "Playing" : state?.status === "paused" ? "Paused" : state ? "Idle" : "Status unavailable";
  }
  function clientAddress(client: SnapClient) { return client.ip_address || client.hostname || "Address unavailable"; }
  function visiblePlayers() { return players.filter(player => player.kind !== "snapcast"); }
  function zonePlayers(zoneId: string | null) { return visiblePlayers().filter(player => (player.zone_id ?? null) === zoneId); }
  function zoneClients(zoneId: string | null) { return snapClients.filter(client => (client.zone_id ?? null) === zoneId); }
  function zoneMemberCount(zoneId: string | null) { return zonePlayers(zoneId).length + zoneClients(zoneId).length; }
  function clientStateLabel(client: SnapClient) {
    if (!client.connected) return "Offline";
    if (snapcastControl !== "connected") return "Connection unavailable";
    const state = states[client.zone_id ? `zone:${client.zone_id}` : "snapcast-local"];
    return state?.status === "playing" ? "Playing" : state?.status === "paused" ? "Paused" : state ? "Idle" : "Status unavailable";
  }

  async function addPlayer(event: SubmitEvent) {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const data = new FormData(form);
    const address = String(data.get("address") ?? "").trim();
    const name = String(data.get("name") ?? "").trim();
    if (!address) return;
    busy = true; setStatus("Connecting…");
    try { await api.createPlayer({ kind: "mpd", address, name: name || undefined }); form.reset(); setStatus("Output added."); await load(); }
    catch (e) { fail(e); } finally { busy = false; }
  }
  async function configureProcessor(player: Player) {
    const current = processors[player.id];
    const host = await promptText({title: "Output processor", label: "CamillaDSP hostname or IP (empty to disconnect)", value: current?.host ?? ""});
    if (host == null) return;
    const port = host ? await promptText({title: "Output processor", label: "WebSocket port", value: String(current?.port ?? 1234)}) : "1234";
    if (port == null) return;
    try { processors[player.id] = await api.saveDspProcessor(player.id, host ? {host, port:Number(port)} : null); } catch (e) { fail(e); }
  }
  async function renamePlayer(player: Player) {
    const name = await promptText({ title: "Rename output", label: "Name", value: player.name });
    if (name == null || !name.trim()) return;
    try { await api.updatePlayer(player.id, { name: name.trim() }); await load(); } catch (e) { fail(e); }
  }
  function manageZone(zone: Zone) {
    editingZoneId = zone.id;
    zoneName = zone.name;
    zoneMembership = Object.fromEntries([
      ...visiblePlayers().map(player => [`player:${player.id}`, player.zone_id === zone.id]),
      ...snapClients.map(client => [`client:${client.id}`, client.zone_id === zone.id]),
    ]);
  }
  function setMembership(key: string, checked: boolean) { zoneMembership = {...zoneMembership, [key]: checked}; }
  async function saveZone(zone: Zone) {
    const name = zoneName.trim();
    if (!name) { setStatus("Enter a zone name.", true); return; }
    if (busy) return;
    busy = true;
    try {
      if (name !== zone.name) await api.updateZone(zone.id, name);
      for (const player of visiblePlayers()) {
        const selected = zoneMembership[`player:${player.id}`] ?? false;
        if (selected !== (player.zone_id === zone.id)) await api.updatePlayer(player.id, {zone_id: selected ? zone.id : null});
      }
      for (const client of snapClients) {
        const selected = zoneMembership[`client:${client.id}`] ?? false;
        if (selected !== (client.zone_id === zone.id)) await api.updateSnapcastClient(client.id, {zone_id: selected ? zone.id : null});
      }
      editingZoneId = null;
      setStatus("Zone saved.");
      await load();
    } catch (e) { fail(e); } finally { busy = false; }
  }
  async function removePlayer(player: Player) {
    if (!(await confirmAction({title: "Remove output", message: `Remove the output “${player.name}”?`, confirmLabel: "Remove"}))) return;
    try { await api.deletePlayer(player.id); await load(); } catch (e) { fail(e); }
  }
  async function assignZone(player: Player, zoneId: string) {
    if (busy) return;
    busy = true;
    try {
      await api.updatePlayer(player.id, { zone_id: zoneId || null });
      setStatus("Zone updated.");
      await load();
    } catch (e) { fail(e); } finally { busy = false; }
  }
  async function assignClientZone(client: SnapClient, zoneId: string) {
    if (busy) return;
    busy = true;
    try { await api.updateSnapcastClient(client.id, { zone_id: zoneId || null }); setStatus("Zone updated."); await load(); } catch (e) { fail(e); } finally { busy = false; }
  }
  async function renameClient(client: SnapClient) {
    const name = await promptText({title: "Rename speaker", label: "Name", value: client.name});
    if (name == null || !name.trim()) return;
    try { await api.updateSnapcastClient(client.id, {name: name.trim()}); await load(); } catch (e) { fail(e); }
  }
  async function addZone(event: SubmitEvent) {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const name = String(new FormData(form).get("name") ?? "").trim();
    if (!name) return;
    if (busy) return;
    busy = true;
    try { await api.createZone(name); form.reset(); setStatus("Zone created. Add outputs from their zone menus."); await load(); } catch (e) { fail(e); } finally { busy = false; }
  }
  async function removeZone(zone: Zone) {
    if (!(await confirmAction({title: `Delete zone ${zone.name}?`, message: "Its outputs become unassigned. They are not removed.", confirmLabel: "Delete zone"}))) return;
    if (busy) return;
    busy = true;
    try { await api.deleteZone(zone.id); setStatus("Zone deleted. Its outputs are now unassigned. Network players briefly reconnect while their audio zone is removed."); await load(); } catch (e) { fail(e); } finally { busy = false; }
  }
</script>

<section class="admin-panel players-panel zones-panel" aria-labelledby="zones-heading">
  <div class="admin-panel-head zones-panel-head"><div><h2 id="zones-heading">Zones &amp; players</h2><p class="admin-hint">Choose which outputs Musicata controls together.</p></div></div>
  {#if loadError}<p class="form-status error" role="alert">{loadError}</p>{/if}

  {#each zones as zone (zone.id)}
    {@const members = zonePlayers(zone.id)}
    {@const clients = zoneClients(zone.id)}
    {@const zoneState = states[`zone:${zone.id}`]}
    <section class="zone-card" aria-labelledby="zone-{zone.id}">
      <div class="zone-card-head"><div><h3 id="zone-{zone.id}">{zone.name}</h3><p>{zoneMemberCount(zone.id)} {zoneMemberCount(zone.id) === 1 ? "player" : "players"} · {zoneState?.status === "playing" ? "Playing" : zoneState?.status === "paused" ? "Paused" : zoneState ? "Idle" : "Status unavailable"}</p></div><div class="zone-actions"><button type="button" class="ghost-button" disabled={busy} onclick={() => manageZone(zone)}>Manage zone</button><button type="button" class="ghost-button danger" disabled={busy} onclick={() => removeZone(zone)}>Delete</button></div></div>
      {#if editingZoneId === zone.id}
        <form class="zone-editor" onsubmit={(event) => { event.preventDefault(); void saveZone(zone); }}>
          <label class="field field-name"><span>Zone name</span><input bind:value={zoneName} maxlength="64" required disabled={busy} /></label>
          <h4>Players in this zone</h4>
          <div class="membership-list">
            {#each visiblePlayers() as player (player.id)}<label><input type="checkbox" checked={zoneMembership[`player:${player.id}`] ?? false} disabled={busy} onchange={(event) => setMembership(`player:${player.id}`, event.currentTarget.checked)} /> <span>{player.name}<small>{player.zone_id && player.zone_id !== zone.id ? `Moves from ${zones.find(choice => choice.id === player.zone_id)?.name ?? "another zone"}` : player.zone_id === zone.id ? "Current member" : "No zone"}</small></span></label>{/each}
            {#each snapClients as client (client.id)}<label><input type="checkbox" checked={zoneMembership[`client:${client.id}`] ?? false} disabled={busy} onchange={(event) => setMembership(`client:${client.id}`, event.currentTarget.checked)} /> <span>{client.name}<small>{!client.connected ? "Offline · remains assigned until it reconnects" : client.zone_id && client.zone_id !== zone.id ? `Moves from ${zones.find(choice => choice.id === client.zone_id)?.name ?? "another zone"}` : client.zone_id === zone.id ? "Current member" : "No zone"}</small></span></label>{/each}
          </div>
          <p class="admin-hint">Selected players join this zone. Clearing a current member leaves it without a zone.</p>
          <p class="admin-hint">Adding the first speaker to a new audio zone briefly reconnects network players.</p>
          <div class="field-actions"><button type="submit" class="primary-button" disabled={busy}>{busy ? "Saving…" : "Save zone"}</button><button type="button" class="ghost-button" disabled={busy} onclick={() => (editingZoneId = null)}>Cancel</button></div>
        </form>
      {/if}
      <div class="zone-members">
        {#each members as player (player.id)}
          <div class="admin-row zone-member">
            <div class="zone-member-title"><span class="dot" class:online={player.online} class:offline={!player.online}></span><div><strong>{player.name}</strong><span class="member-detail">{player.address}</span></div><span class="member-state" class:offline={!player.online}>{stateLabel(player)}</span></div>
            <label class="zone-member-picker"><span>Zone</span><select disabled={busy} aria-label="Zone for {player.name}" value={player.zone_id ?? ""} onchange={(event) => assignZone(player, event.currentTarget.value)}><option value="">No zone</option>{#each zones as choice (choice.id)}<option value={choice.id}>{choice.name}</option>{/each}</select></label>
            {#if player.kind === "mpd"}<button type="button" class="ghost-button" onclick={() => configureProcessor(player)}>{processors[player.id] ? "Processor" : "Connect processor"}</button>{/if}
            {#if player.kind !== "browser"}<button type="button" class="ghost-button" onclick={() => renamePlayer(player)}>Rename</button><button type="button" class="ghost-button danger" onclick={() => removePlayer(player)}>Remove</button>{/if}
          </div>
        {/each}
        {#each clients as client (client.id)}
          <div class="admin-row zone-member snapcast-client"><div class="zone-member-title"><span class="dot" class:online={client.connected} class:offline={!client.connected}></span><div><strong>{client.name}</strong><span class="member-detail">{clientAddress(client)} · Snapcast speaker</span></div><span class="member-state" class:offline={!client.connected}>{clientStateLabel(client)}</span></div><label class="zone-member-picker"><span>Zone</span><select disabled={busy} aria-label="Zone for {client.name}" value={client.zone_id ?? ""} onchange={(event) => assignClientZone(client, event.currentTarget.value)}><option value="">No zone</option>{#each zones as choice (choice.id)}<option value={choice.id}>{choice.name}</option>{/each}</select></label><button type="button" class="ghost-button" disabled={busy} onclick={() => renameClient(client)}>Rename</button></div>
        {/each}
        {#if !members.length && !clients.length}<p class="admin-hint zone-empty">No players in this zone yet. Assign one below.</p>{/if}
      </div>
    </section>
  {:else}<p class="admin-hint zone-empty">Create a zone to control a set of outputs together.</p>{/each}

  <form class="field-form zone-create" onsubmit={addZone}><h3>Create a zone</h3><div class="field-actions"><label class="field field-name"><span>Name</span><input name="name" placeholder="Upstairs" required /></label><button type="submit" class="primary-button" disabled={busy}>Create zone</button></div><p class="admin-hint">Adding the first speaker to a new audio zone briefly reconnects network players.</p></form>

  <section class="unassigned-outputs" aria-labelledby="unassigned-heading"><h3 id="unassigned-heading">Players without a zone</h3><p class="admin-hint">Assign an output to a zone when it should receive that zone’s controls and queue.</p>
    {#each zonePlayers(null) as player (player.id)}
      <div class="admin-row zone-member unassigned-member"><div class="zone-member-title"><span class="dot" class:online={player.online} class:offline={!player.online}></span><div><strong>{player.name}</strong><span class="member-detail">{player.address}</span></div><span class="member-state" class:offline={!player.online}>{stateLabel(player)}</span></div><label class="zone-member-picker"><span>Zone</span><select disabled={busy} aria-label="Zone for {player.name}" value="" onchange={(event) => assignZone(player, event.currentTarget.value)}><option value="">No zone</option>{#each zones as choice (choice.id)}<option value={choice.id}>{choice.name}</option>{/each}</select></label>{#if player.kind === "mpd"}<button type="button" class="ghost-button" onclick={() => configureProcessor(player)}>{processors[player.id] ? "Processor" : "Connect processor"}</button>{/if}{#if player.kind !== "browser"}<button type="button" class="ghost-button" onclick={() => renamePlayer(player)}>Rename</button><button type="button" class="ghost-button danger" onclick={() => removePlayer(player)}>Remove</button>{/if}</div>
    {/each}
    {#each zoneClients(null) as client (client.id)}
      <div class="admin-row zone-member unassigned-member snapcast-client"><div class="zone-member-title"><span class="dot" class:online={client.connected} class:offline={!client.connected}></span><div><strong>{client.name}</strong><span class="member-detail">{clientAddress(client)} · Snapcast speaker</span></div><span class="member-state" class:offline={!client.connected}>{clientStateLabel(client)}</span></div><label class="zone-member-picker"><span>Zone</span><select disabled={busy} aria-label="Zone for {client.name}" value="" onchange={(event) => assignClientZone(client, event.currentTarget.value)}><option value="">No zone</option>{#each zones as choice (choice.id)}<option value={choice.id}>{choice.name}</option>{/each}</select></label><button type="button" class="ghost-button" disabled={busy} onclick={() => renameClient(client)}>Rename</button></div>
    {/each}
    {#if zoneMemberCount(null) === 0}<p class="admin-hint zone-empty">Every player belongs to a zone.</p>{/if}
  </section>

  <details class="add-output"><summary>Add an MPD output</summary><form class="field-form" onsubmit={addPlayer}><div class="field-grid"><label class="field field-addr"><span>Address</span><input name="address" placeholder="127.0.0.1:6600" required /></label><label class="field field-name"><span>Name <em>(optional)</em></span><input name="name" placeholder="Living room" /></label></div><div class="field-actions"><button type="submit" class="primary-button" disabled={busy}>Add output</button></div></form></details>
  {#if status}<p class="form-status" class:error role="status">{status}</p>{/if}
</section>

<style>
  .zones-panel-head { align-items: flex-start; } .zones-panel-head .admin-hint { margin: .25rem 0 0; }
  .zone-card { border-top: 1px solid var(--line); padding: 1rem 0; } .zone-card-head, .zone-member, .snapcast-client { display: flex; align-items: center; gap: .7rem; } .zone-card-head { justify-content: space-between; }
  .zone-card h3, .unassigned-outputs h3 { margin: 0; font-size: 1rem; } .zone-card-head p, .member-detail { display: block; color: var(--muted); font-size: .78rem; } .zone-actions { display: flex; gap: .45rem; flex-wrap: wrap; } .zone-members { margin-top: .8rem; }
  .zone-member { min-width: 0; padding: .7rem 0; border-top: 1px solid color-mix(in srgb, var(--line) 70%, transparent); flex-wrap: wrap; } .zone-member-title { display: flex; align-items: center; gap: .6rem; flex: 1 1 12rem; min-width: 0; } .zone-member-title strong, .snapcast-client strong { overflow-wrap: anywhere; }
  .member-state { color: var(--muted); font-size: .78rem; margin-left: auto; white-space: nowrap; } .member-state.offline { color: var(--danger); } .zone-member-picker { display: grid; gap: .2rem; color: var(--muted); font-size: .72rem; } .zone-member-picker select { min-width: 9rem; }
  .snapcast-client { padding: .45rem 0; } .snapcast-client > div { min-width: 0; } .zone-empty { margin: .8rem 0 0; }
  .zone-editor { margin: .9rem 0; padding: 1rem; background: var(--panel-strong); border-radius: 10px; } .zone-editor h4 { margin: 1rem 0 .45rem; font-size: .9rem; } .membership-list { display: grid; gap: .35rem; } .membership-list label { display: flex; align-items: center; gap: .55rem; min-height: 2.5rem; } .membership-list small { display: block; color: var(--muted); font-size: .75rem; } .zone-create { margin-top: .2rem; } .zone-create .field-actions { align-items: end; } .zone-create .primary-button { white-space: nowrap; } .unassigned-outputs { border-top: 1px solid var(--line); margin-top: 1.2rem; padding-top: 1rem; } .unassigned-member { margin-top: .5rem; } .add-output { border-top: 1px solid var(--line); margin-top: 1.1rem; padding-top: .8rem; } .add-output summary { cursor: pointer; color: var(--muted); } .add-output[open] summary { color: var(--text); }
  @media (max-width: 720px) { .zone-card-head { align-items: flex-start; flex-direction: column; } .zone-member-picker { flex: 1 1 100%; } .zone-member-picker select { width: 100%; } }
</style>
