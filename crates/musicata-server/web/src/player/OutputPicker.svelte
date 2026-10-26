<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { tick } from "svelte";
  import { api, ApiError } from "../lib/api";
  import type { Player } from "../types/Player";
  import type { SnapcastStatus } from "../types/SnapcastStatus";
  import type { Zone } from "../types/Zone";
  import { player } from "../lib/player.svelte";
  import { session } from "../lib/session.svelte";
  let { players, zones, memberId, onmemberchange, onchange, onrefresh }: {
    players: Player[]; zones: Zone[]; memberId: string | null;
    onmemberchange: (member: Player) => void; onchange: (value: string) => void; onrefresh: () => Promise<boolean>;
  } = $props();
  let trigger: HTMLButtonElement;
  let panel: HTMLDivElement;
  let open = $state(false);
  let snapcast = $state<SnapcastStatus | null>(null);
  let snapcastExpanded = $state(false);
  let expandedSnapcastGroupId = $state<string | null>(null);
  let editingSnapcastGroupId = $state<string | null>(null);
  let draftSnapcastMembers = $state<string[]>([]);
  let snapcastError = $state("");
  let snapcastBusy = $state(false);
  let snapcastVolumes = $state<Record<string, number>>({});
  const current = $derived((player.activeKind === "zone" ? zones : players).find(p => p.id === player.activeId));
  const members = $derived(players.filter(p => p.zone_id === player.activeId));
  // Per-zone stream players are internal. The default unassigned Snapcast output
  // remains available until speakers have been placed in a Musicata zone.
  const standalonePlayers = $derived(players.filter(p => !p.zone_id && !p.id.startsWith("snapcast-zone-")));
  function place() {
    if (!panel || !trigger) return;
    const r = trigger.getBoundingClientRect();
    panel.style.left = `${Math.max(8, Math.min(r.left, innerWidth - Math.min(360, innerWidth - 16) - 8))}px`;
    panel.style.bottom = `${innerHeight - r.top + 8}px`;
    panel.style.maxHeight = `${Math.max(100, r.top - 16)}px`;
  }
  async function toggled(event: ToggleEvent) {
    open = event.newState === "open";
    if (open) {
      place();
      await onrefresh();
      await loadSnapcast();
      await tick();
      if (!open) return;
      panel.querySelector<HTMLButtonElement>('[aria-pressed="true"]')?.focus({preventScroll: true});
    }
  }
  function choose(value: string) {
    if (value !== `${player.activeKind}:${player.activeId}`) onchange(value);
    panel.hidePopover();
    trigger.focus({preventScroll: true});
  }
  async function loadSnapcast() {
    if (!players.some(p => p.kind === "snapcast")) return;
    try {
      const next = await api.snapcastStatus();
      snapcast = next;
      snapcastVolumes = Object.fromEntries(next.clients.map(client => [client.id, client.volume_percent]));
      snapcastError = "";
    } catch {
      // The Snapcast cargo feature can be absent, or its service can be unavailable.
      snapcast = null;
    }
  }
  async function setSnapcastVolume(clientId: string) {
    const percent = snapcastVolumes[clientId];
    if (percent == null) return;
    try {
      await api.setSnapcastVolume(clientId, percent);
    } catch {
      await loadSnapcast();
    }
  }
  async function updateSnapcastGroup(groupId: string, update: { client_ids?: string[]; stream_id?: string }) {
    if (!session.isAdmin || !snapcast) return;
    snapcastBusy = true;
    snapcastError = "";
    try {
      snapcast = await api.updateSnapcastGroup(groupId, update);
      if (update.client_ids) editingSnapcastGroupId = null;
    } catch (error) {
      snapcastError = error instanceof ApiError ? error.message : "Could not change the stream.";
    } finally {
      snapcastBusy = false;
    }
  }
  function editSnapcastMembers(groupId: string, members: string[]) {
    editingSnapcastGroupId = groupId;
    draftSnapcastMembers = [...members];
  }
  function toggleSnapcastMember(clientId: string, checked: boolean) {
    draftSnapcastMembers = checked
      ? [...draftSnapcastMembers, clientId]
      : draftSnapcastMembers.filter(id => id !== clientId);
  }
</script>

<svelte:window onresize={() => { if (open) place(); }} />
<button bind:this={trigger} class="output-trigger" type="button" popovertarget="output-picker"
  aria-label={`Choose output: ${current?.name ?? "Connecting…"}`} aria-expanded={open} aria-haspopup="dialog">
  <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="5" y="2" width="14" height="20" rx="3"/><circle cx="12" cy="15" r="3"/><path d="M11 7h2"/></svg>
  <span class="output-caption"><span>Playing on</span><strong>{current?.name ?? "Connecting…"}</strong></span>
  <span class="output-chevron" aria-hidden="true">⌃</span>
</button>
<div bind:this={panel} id="output-picker" class="output-picker" popover="auto" role="dialog" tabindex="-1" aria-label="Choose output" ontoggle={toggled}
  onkeydown={event => { if (event.key === "Escape") { event.stopPropagation(); panel.hidePopover(); trigger.focus({preventScroll: true}); } }}>
  <header><h2>Play on</h2><button class="ghost-button" type="button" aria-label="Close output picker" onclick={() => {panel.hidePopover(); trigger.focus();}}>×</button></header>
  <p class="output-hint">Choose where to control playback. Selecting a destination never moves speakers or transfers a queue.</p>
  <div role="group" aria-label="Players">
    {#each standalonePlayers as p (p.id)}
      {#if p.kind === "snapcast" && snapcastExpanded}
        <div class="output-group">
          <div class="output-group-head">
            <button class="output-option" type="button" data-output={`player:${p.id}`} aria-pressed={player.activeKind === "player" && player.activeId === p.id} onclick={() => choose(`player:${p.id}`)}>
              <span><strong>{p.name}</strong><small>Multi-room audio · {p.online ? "Available" : "Offline"}</small></span>
              {#if player.activeKind === "player" && player.activeId === p.id}<span aria-hidden="true">✓</span>{/if}
            </button>
            <button class="ghost-button" type="button" aria-expanded={snapcastExpanded} aria-controls="snapcast-details" onclick={() => (snapcastExpanded = !snapcastExpanded)}>
              {snapcastExpanded ? "Hide speakers" : "Show speakers"}
            </button>
          </div>
          <div class="output-details">
          {#if snapcastExpanded}
            <section id="snapcast-details" aria-label="Multi-room speaker details">
              {#if snapcast?.running}
                <p class="output-hint">Each group is a real Snapcast destination. Musicata has one shared library queue; groups routed to Library hear that same queue.</p>
                {#each snapcast.groups as group (group.id)}
                  <section class="output-snapcast-group">
                    <div class="output-group-head">
                      <button class="output-option" type="button" data-snapcast-group={group.id} aria-expanded={expandedSnapcastGroupId === group.id} onclick={() => (expandedSnapcastGroupId = expandedSnapcastGroupId === group.id ? null : group.id)}>
                        <span><strong>{group.name}</strong><small>{group.clients.map(client => client.name).join(" · ") || "No speakers"} · {group.stream_id === "Musicata" ? "Library queue" : group.stream_id}</small></span>
                      </button>
                      <button class="ghost-button" type="button" aria-label={`Control Musicata on ${group.name}`} onclick={() => choose(`player:${p.id}`)}>Control</button>
                    </div>
                    {#if expandedSnapcastGroupId === group.id}
                      {#each group.clients as client (client.id)}
                        <div class="output-speaker">
                          <span><strong>{client.name}</strong><small>{client.connected ? (client.muted ? "Muted" : "Connected") : "Offline"}</small></span>
                          <label>
                            <span class="sr-only">{client.name} volume</span>
                            <input type="range" min="0" max="100" value={snapcastVolumes[client.id] ?? client.volume_percent} disabled={!client.connected} oninput={event => {
                              snapcastVolumes[client.id] = Number(event.currentTarget.value);
                            }} onchange={() => void setSnapcastVolume(client.id)} />
                            <output>{snapcastVolumes[client.id] ?? client.volume_percent}%</output>
                          </label>
                        </div>
                      {/each}
                      <div class="output-routing">
                        <strong>What this group hears</strong>
                        {#if session.isAdmin}
                          <label><span class="sr-only">Stream for {group.name}</span>
                            <select value={group.stream_id} disabled={snapcastBusy} onchange={event => void updateSnapcastGroup(group.id, { stream_id: event.currentTarget.value })}>
                              {#each snapcast.inputs as input (input.id)}
                                <option value={input.id}>{input.id === "Musicata" ? "Library queue" : input.id}{input.status === "playing" ? " · Playing" : ""}</option>
                              {/each}
                            </select>
                          </label>
                        {:else}
                          <small>{group.stream_id === "Musicata" ? "Library queue" : group.stream_id}. Only an administrator can change routing.</small>
                        {/if}
                        <small>Changing the stream keeps this group’s speakers and the Musicata queue unchanged.</small>
                      </div>
                      <div class="output-membership">
                        <strong>Speakers in this group</strong>
                        {#if session.isAdmin}
                          {#if editingSnapcastGroupId === group.id}
                            {#each snapcast.clients as client (client.id)}
                              <label class="output-member-choice"><input type="checkbox" checked={draftSnapcastMembers.includes(client.id)} onchange={event => toggleSnapcastMember(client.id, event.currentTarget.checked)} /> {client.name}</label>
                            {/each}
                            <button class="ghost-button" type="button" disabled={snapcastBusy || !draftSnapcastMembers.length} onclick={() => void updateSnapcastGroup(group.id, { client_ids: draftSnapcastMembers })}>Apply speakers</button>
                          {:else}
                            <button class="ghost-button" type="button" onclick={() => editSnapcastMembers(group.id, group.clients.map(client => client.id))}>Edit speakers</button>
                          {/if}
                        {:else}
                          <small>Only an administrator can change speaker membership.</small>
                        {/if}
                        <small>Speaker membership is Snapcast transport membership, separate from Musicata zones.</small>
                      </div>
                    {/if}
                  </section>
                {/each}
                {#if !snapcast.groups.length}<p class="output-hint">No speaker groups are connected.</p>{/if}
                {#if snapcastError}<p role="alert">{snapcastError}</p>{/if}
              {:else}
                <p class="output-hint">Multi-room is not running. An administrator can configure it in Settings.</p>
              {/if}
            </section>
          {/if}
          </div>
        </div>
      {:else}
        <button class="output-option" type="button" data-output={`player:${p.id}`} aria-pressed={player.activeKind === "player" && player.activeId === p.id} onclick={() => choose(`player:${p.id}`)}>
          <span><strong>{p.name}</strong><small>{p.kind === "browser" ? "This device" : p.kind === "mpd" ? "MPD player" : "Audio player"} · {p.online ? "Available" : "Offline"}</small></span>
          {#if player.activeKind === "player" && player.activeId === p.id}<span aria-hidden="true">✓</span>{/if}
        </button>
      {/if}
    {/each}
  </div>
  {#if zones.length}
    <h3>Zones</h3>
    <div role="group" aria-label="Zones">
      {#each zones as zone (zone.id)}
        <button class="output-option" type="button" data-output={`zone:${zone.id}`} aria-pressed={player.activeKind === "zone" && player.activeId === zone.id} onclick={() => choose(`zone:${zone.id}`)}>
          <span><strong>{zone.name}</strong><small>Musicata zone</small></span>
          {#if player.activeKind === "zone" && player.activeId === zone.id}<span aria-hidden="true">✓</span>{/if}
        </button>
      {/each}
    </div>
  {/if}
  {#if player.activeKind === "zone" && members.length}
    <label class="output-member">EQ and meter member
      <select aria-label="EQ and meter member" value={memberId ?? ""} onchange={event => {
        const member = members.find(p => p.id === event.currentTarget.value);
        if (member) onmemberchange(member);
      }}>
        {#each members as member (member.id)}<option value={member.id}>{member.name}</option>{/each}
      </select>
    </label>
  {/if}
</div>

<style>
  .output-trigger { display:flex; align-items:center; gap:10px; min-width:0; width:100%; min-height:48px; padding:6px 12px; color:var(--text); background:var(--panel-strong); border:1px solid var(--line); border-radius:12px; cursor:pointer; text-align:left; }
  .output-trigger:hover, .output-trigger[aria-expanded="true"] { border-color:var(--accent); }
  svg { width:22px; height:22px; flex:none; fill:none; stroke:var(--accent); stroke-width:1.5; }
  .output-caption { display:grid; min-width:0; flex:1; }
  .output-caption > span { font-size:.68rem; color:var(--muted); }
  .output-caption strong { font-size:.85rem; overflow:hidden; white-space:nowrap; text-overflow:ellipsis; }
  .output-chevron { color:var(--accent); }
  .output-picker { position:fixed; inset:auto; margin:0; width:min(360px, calc(100vw - 16px)); overflow:auto; padding:12px; border:1px solid var(--line); border-radius:16px; background:var(--panel); color:var(--text); box-shadow:0 12px 40px #0008; }
  header { display:flex; align-items:center; justify-content:space-between; gap:12px; }
  h2 { font-family:var(--font-display); margin:0; font-size:1.3rem; }
  header button { min-width:44px; min-height:44px; }
  .output-hint, h3 { color:var(--muted); font-size:.8rem; margin:8px 0; }
  .output-option { display:flex; align-items:center; justify-content:space-between; gap:12px; width:100%; min-height:60px; padding:10px 12px; border:1px solid transparent; border-radius:10px; background:none; color:var(--text); text-align:left; cursor:pointer; }
  .output-option:hover { background:var(--panel-strong); }
  .output-option[aria-pressed="true"] { border-color:var(--accent); background:var(--accent-soft); }
  .output-option > span:first-child { min-width:0; overflow-wrap:anywhere; }
  .output-option strong, .output-option small { display:block; }
  .output-option small { margin-top:4px; color:var(--muted); }
  .output-member { display:grid; gap:6px; border-top:1px solid var(--line); margin-top:12px; padding-top:12px; font-size:.85rem; }
  .output-details { padding:0 12px 12px; }
  .output-group-head { display:flex; align-items:center; gap:4px; }
  .output-group-head .output-option { flex:1; }
  .output-snapcast-group { border-top:1px solid var(--line); margin-top:8px; padding-top:8px; }
  .output-speaker { display:flex; align-items:center; justify-content:space-between; gap:12px; padding:10px 0; border-bottom:1px solid var(--line); }
  .output-speaker > span { min-width:0; }
  .output-speaker strong, .output-speaker small { display:block; }
  .output-speaker small, .output-routing small, .output-membership small { color:var(--muted); }
  .output-speaker label { display:flex; align-items:center; gap:6px; }
  .output-speaker input { width:100px; }
  .output-speaker output { min-width:3ch; font-variant-numeric:tabular-nums; }
  .output-routing, .output-membership { display:grid; gap:6px; padding-top:12px; font-size:.85rem; }
  .output-member-choice { display:flex; align-items:center; gap:8px; }
  .output-routing select { min-height:36px; }
  .sr-only { position:absolute; width:1px; height:1px; padding:0; overflow:hidden; clip:rect(0, 0, 0, 0); white-space:nowrap; border:0; }
  select { min-width:0; width:100%; min-height:44px; }
  @media(max-width:1023px) {
    .output-trigger { min-height:40px; height:40px; border-radius:8px; padding:2px 10px; gap:8px; }
    .output-caption { display:flex; align-items:baseline; gap:8px; }
    .output-caption > span { flex:none; font-size:.75rem; }
  }
</style>
