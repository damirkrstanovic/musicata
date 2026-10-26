<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { tick } from "svelte";
  import { api, ApiError, type Player, type Zone } from "../lib/api";
  import type { SnapcastStatus } from "../types/SnapcastStatus";

  let {
    players,
    zones,
    snapcast: initialSnapcast,
    oncomplete,
  }: {
    players: Player[];
    zones: Zone[];
    snapcast: SnapcastStatus | null;
    oncomplete: () => Promise<void>;
  } = $props();

  type Kind = "snapcast" | "mpd" | "browser";
  let open = $state(false);
  let step = $state(0);
  let kind = $state<Kind>("snapcast");
  let name = $state("");
  let host = $state("");
  let port = $state(6600);
  let password = $state("");
  let zone = $state("");
  let newZone = $state("");
  let snapcast = $state<SnapcastStatus | null>(null);
  let provision = $state<{ id: string; command: string; auth_enabled: boolean } | null>(null);
  let snapConnected = $state(false);
  let mpdAddress = $state("");
  let checked = $state(false);
  let createdZoneId = $state<string | null>(null);
  let createdPlayerId = $state<string | null>(null);
  let busy = $state(false);
  let message = $state("");
  let failed = $state(false);
  let title: HTMLElement | undefined = $state();
  const browser = $derived(players.find(player => player.kind === "browser") ?? null);
  const serverHost = $derived(
    snapcast?.server_host && !["localhost", "127.0.0.1", "::1"].includes(snapcast.server_host)
      ? snapcast.server_host
      : window.location.hostname,
  );
  const continueEnabled = $derived(kind === "browser" || (kind === "mpd" ? checked : snapConnected));
  const errorText = (e: unknown) =>
    e instanceof ApiError || e instanceof Error ? e.message : "That change could not be saved. Try again.";

  async function focusTitle() {
    await tick();
    title?.focus();
  }

  function start() {
    snapcast = initialSnapcast;
    open = true;
    step = 0;
    kind = "snapcast";
    name = "";
    host = "";
    port = 6600;
    password = "";
    zone = "";
    newZone = "";
    provision = null;
    snapConnected = false;
    mpdAddress = "";
    checked = false;
    createdZoneId = null;
    createdPlayerId = null;
    message = "";
    failed = false;
    void focusTitle();
  }

  async function cancel() {
    if (provision && !snapConnected) {
      busy = true;
      try {
        const current = await api.snapcastStatus();
        if (current.health.control !== "connected") {
          message = "The player identity was kept because Snapcast connection status is unavailable.";
          failed = true;
          return;
        }
        const connected = current.health.control === "connected" && current.health.clients.some(client => client.id === provision?.id && client.connected);
        if (connected) {
          snapConnected = true;
          message = "This player connected while this page was open. It remains in Zones & players.";
          failed = false;
          return;
        }
        await api.deleteSnapcastRoom(provision.id);
      } catch (e) {
        message = `The unused player identity was kept because its connection could not be checked: ${errorText(e)}`;
        failed = true;
        return;
      } finally {
        busy = false;
      }
    }
    open = false;
  }

  function choose(next: Kind) {
    if (provision && next !== "snapcast") return;
    kind = next;
    checked = false;
    message = "";
    failed = false;
    if (next === "browser") {
      name = browser?.name ?? "This browser";
      zone = browser?.zone_id ?? "";
    }
  }

  async function next() {
    if (step === 0) {
      step = 1;
      if (kind === "browser") {
        name = browser?.name ?? "This browser";
        zone = browser?.zone_id ?? "";
      }
      await focusTitle();
      return;
    }
    if (continueEnabled) {
      step = 2;
      if (!name) name = kind === "mpd" ? "MPD player" : "Snapcast speaker";
      await focusTitle();
    }
  }

  async function enable() {
    busy = true;
    try {
      snapcast = await api.updateSnapcast({ enabled: true });
      message = "Snapcast enabled.";
      failed = false;
    } catch (e) {
      message = errorText(e);
      failed = true;
    } finally {
      busy = false;
    }
  }

  async function prepare() {
    busy = true;
    message = "";
    failed = false;
    try {
      provision ??= await api.provisionSnapcastPlayer(host.trim() || serverHost);
    } catch (e) {
      message = errorText(e);
      failed = true;
    } finally {
      busy = false;
    }
  }

  async function checkSnapcast() {
    if (!provision) return;
    busy = true;
    message = "Checking for this player…";
    failed = false;
    try {
      const current = await api.snapcastStatus();
      snapcast = current;
      const client = current.health.clients.find(client => client.id === provision?.id && client.connected);
      snapConnected = current.health.control === "connected" && !!client;
      if (!snapConnected || !client) throw new Error("This player has not connected yet. Run its command on the player device, then try again.");
      message = `Connected: ${client.name} · ${client.ip_address || client.hostname}. Connection is confirmed; sound has not been checked.`;
    } catch (e) {
      message = errorText(e);
      failed = true;
      snapConnected = false;
    } finally {
      busy = false;
    }
  }

  async function checkMpd() {
    if (!host.trim() || port < 1 || port > 65535) {
      message = "Enter a hostname or IP address and a port from 1 to 65535.";
      failed = true;
      return;
    }
    const request = { host: host.trim(), port, password: password || undefined };
    busy = true;
    message = "Testing connection…";
    failed = false;
    try {
      const result = await api.probePlayer(request);
      if (host.trim() !== request.host || port !== request.port || (password || undefined) !== request.password) return;
      if (!result?.connected) throw new Error("Could not reach this MPD player.");
      mpdAddress = result.address;
      checked = true;
      message = "Connection confirmed. Sound has not been checked.";
    } catch (e) {
      message = errorText(e);
      failed = true;
      checked = false;
    } finally {
      busy = false;
    }
  }

  async function copy() {
    if (!provision) return;
    try {
      await navigator.clipboard.writeText(provision.command);
      message = "Command copied.";
      failed = false;
    } catch {
      message = "Select and copy the command.";
      failed = false;
    }
  }

  async function save() {
    const playerName = name.trim();
    const zoneName = newZone.trim();
    if (!playerName || (zone === "new" && !zoneName)) {
      message = !playerName ? "Enter a player name." : "Enter a zone name.";
      failed = true;
      return;
    }
    busy = true;
    message = "Saving…";
    failed = false;
    try {
      let zoneId: string | null = zone || null;
      if (zone === "new") {
        if (!createdZoneId) {
          const made = await api.createZone(zoneName);
          if (!made) throw new Error("Could not create the zone.");
          createdZoneId = made.id;
        }
        zoneId = createdZoneId;
      }
      if (kind === "mpd") {
        if (!createdPlayerId) {
          const made = await api.createPlayer({ kind: "mpd", address: mpdAddress, name: playerName, password: password || undefined }) as Player | null;
          if (!made) throw new Error("Could not add the player.");
          createdPlayerId = made.id;
        }
        if (!createdPlayerId) throw new Error("Could not add the player.");
        await api.updatePlayer(createdPlayerId, { name: playerName, zone_id: zoneId });
      } else if (kind === "browser") {
        if (!browser) throw new Error("This browser player is unavailable. Refresh and try again.");
        await api.updatePlayer(browser.id, { name: playerName, zone_id: zoneId });
      } else {
        if (!provision || !snapConnected) throw new Error("Confirm the Snapcast connection before saving.");
        await api.updateSnapcastClient(provision.id, { name: playerName, zone_id: zoneId });
      }
      await oncomplete();
      provision = null;
      open = false;
    } catch (e) {
      message = errorText(e);
      failed = true;
    } finally {
      busy = false;
    }
  }
</script>

{#if !open}
  <button type="button" class="primary-button add-player-button" onclick={start}>Add player</button>
{:else}
  <section class="add-player-wizard" aria-labelledby="add-player-heading">
    <header><div><h3 id="add-player-heading" bind:this={title} tabindex="-1">Add player</h3><p>Choose a device, connect it, then decide where it plays.</p></div><button type="button" class="ghost-button" disabled={busy} onclick={() => void cancel()}>Cancel</button></header>
    <div class="wizard-layout"><ol class="wizard-steps"><li class:done={step>0} aria-current={step===0?"step":undefined}><b>1</b>Player type</li><li class:done={step>1} aria-current={step===1?"step":undefined}><b>2</b>Connect</li><li aria-current={step===2?"step":undefined}><b>3</b>Name &amp; zone</li></ol><div class="wizard-content">
      {#if step===0}
        <h4>What kind of player?</h4><p class="admin-hint">Choose how this device plays music. You can mix player types in a zone.</p><div class="player-choices" role="radiogroup" aria-label="Player type"><label><input data-player-kind="snapcast" type="radio" name="kind" checked={kind==="snapcast"} disabled={!!provision} onchange={() => choose("snapcast")} /><span><strong>Snapcast speaker <em>Synchronized multi-room</em></strong><small>For a speaker connected to a device running Snapclient. Speakers in one zone play in sync.</small></span></label><label><input data-player-kind="mpd" type="radio" name="kind" checked={kind==="mpd"} disabled={!!provision} onchange={() => choose("mpd")} /><span><strong>MPD player</strong><small>Connect an existing Music Player Daemon by hostname or IP address.</small></span></label><label><input data-player-kind="browser" type="radio" name="kind" checked={kind==="browser"} disabled={!!provision} onchange={() => choose("browser")} /><span><strong>This browser</strong><small>Play through this device’s browser. No installation needed.</small></span></label></div>
      {:else if step===1}
        <h4>Connect</h4>
        {#if kind==="snapcast"}{#if !snapcast?.enabled}<p class="admin-hint">Multi-room playback is disabled. Enable it before preparing a new speaker.</p><button type="button" class="primary-button" disabled={busy} onclick={() => void enable()}>Enable Snapcast</button>{:else}<p class="admin-hint">Set up Snapclient on a new device. Musicata checks only the player identity prepared here.</p><label class="field field-host"><span>Snapcast server host</span><input bind:value={host} placeholder={serverHost} disabled={busy || !!provision} /></label>{#if !provision}<button type="button" class="primary-button" disabled={busy} onclick={() => void prepare()}>Prepare player</button>{:else}<ol class="instructions"><li>Install Snapclient on the player device.</li><li>Run this player’s connection command on that device.</li></ol><div class="command"><code>{provision.command}</code><button type="button" class="ghost-button" onclick={() => void copy()}>Copy command</button></div><details><summary>Connection &amp; authentication details</summary><p>{provision.auth_enabled ? "This command includes this player’s generated credentials." : "Password provisioning is off in the server settings."} Renaming the player later keeps its identity.</p><p>The bundled Snapserver does not yet enforce these credentials, so this command does not prove authenticated access.</p></details><button type="button" class="ghost-button" disabled={busy} onclick={() => void checkSnapcast()}>Check for this player</button>{/if}{/if}
        {:else if kind==="mpd"}<p class="admin-hint">Musicata connects to MPD already running on your device.</p><div class="field-grid"><label class="field field-host"><span>Hostname or IP address</span><input bind:value={host} placeholder="kitchen.local or 192.168.1.36" disabled={busy} oninput={() => checked=false} /></label><label class="field field-port"><span>Port</span><input type="number" min="1" max="65535" bind:value={port} disabled={busy} oninput={() => checked=false} /></label><label class="field field-password"><span>Password <em>(optional)</em></span><input type="password" bind:value={password} placeholder="Only if MPD requires one" disabled={busy} oninput={() => checked=false} /></label></div><button type="button" class="ghost-button" disabled={busy} onclick={() => void checkMpd()}>Test connection</button><details><summary>MPD is not installed yet?</summary><p>Install and configure MPD on the device first. For synchronized multi-room speakers, choose Snapcast.</p></details>
        {:else}<p class="admin-hint">Use the speakers or headphones attached to this device.</p><div class="notice"><strong>This browser is already a player</strong><br />Continue to give it a name and choose a zone. Keep the Musicata tab open while it plays.</div><p class="admin-hint">Browser playback follows zone controls, but is not sample-synchronized with Snapcast speakers.</p>{/if}
        {#if kind === "snapcast" && snapConnected}<p class="admin-hint">Cancelling now leaves this connected player in Zones &amp; players. Its identity is not removed.</p>{/if}
      {:else}
        <h4>Name &amp; zone</h4><p class="admin-hint">This is how the player will appear in Musicata.</p><label class="field field-name"><span>Player name</span><input bind:value={name} maxlength="64" placeholder="Kitchen speaker" /></label><label class="field field-zone"><span>Zone</span><select bind:value={zone} disabled={busy || !!createdZoneId}><option value="">{kind === "snapcast" ? "No zone — shared default playback" : "No zone — play independently"}</option>{#each zones as item (item.id)}<option value={item.id}>{item.name}</option>{/each}<option value="new">Create a new zone…</option></select></label>{#if zone==="new"}<label class="field field-name"><span>New zone name</span><input bind:value={newZone} disabled={busy || !!createdZoneId} maxlength="64" placeholder="Kitchen" /></label>{/if}<div class="notice"><strong>{zone ? "Control these players together" : kind === "snapcast" ? "Shared default playback" : "An independent player"}</strong><br />{zone ? "The player will follow this zone’s queue, play/pause and track changes." : kind === "snapcast" ? "Speakers without a zone share the default queue. Create a zone for independent playback." : "This player keeps its own queue. You can add it to a zone later."}</div>
        {#if kind === "snapcast" && zone}<p class="admin-hint">Adding the first speaker to a zone can briefly reconnect network players.</p>{/if}
      {/if}
      {#if message}<p class="wizard-status" class:failed role={failed?"alert":"status"}>{message}</p>{/if}<footer><button type="button" class="ghost-button" disabled={busy || step===0 || !!createdPlayerId || !!createdZoneId} onclick={() => {step-=1;void focusTitle();}}>Back</button>{#if step<2}<button type="button" class="primary-button" disabled={busy || (step===1 && !continueEnabled)} onclick={() => void next()}>Continue</button>{:else}<button type="button" class="primary-button" disabled={busy} onclick={() => void save()}>{busy ? "Saving…" : kind==="browser" ? "Save player" : "Add player"}</button>{/if}</footer>
    </div></div>
  </section>
{/if}

<style>
  .add-player-button{margin-top:1.2rem}.add-player-wizard{border-top:1px solid var(--line);margin-top:1.2rem;padding-top:1.2rem}.add-player-wizard header{display:flex;justify-content:space-between;gap:1rem}.add-player-wizard h3,.wizard-content h4{font-family:var(--font-display);font-size:1.35rem;font-weight:400;margin:0}.add-player-wizard header p{color:var(--muted);margin:.3rem 0 0}.wizard-layout{border-top:1px solid var(--line);display:grid;gap:2.5rem;grid-template-columns:11rem minmax(0,42rem);align-items:start;margin-top:1.25rem;padding-top:1.25rem}.wizard-steps{color:var(--muted);display:grid;gap:.85rem;list-style:none;margin:0;padding:0}.wizard-steps li{align-items:center;display:flex;gap:.55rem}.wizard-steps b{border:1px solid var(--line);border-radius:50%;display:grid;font-size:.75rem;height:1.65rem;place-items:center;width:1.65rem}.wizard-steps [aria-current="step"]{color:var(--text)}.wizard-steps [aria-current="step"] b{background:var(--accent);border-color:var(--accent);color:var(--bg)}.wizard-steps .done b{color:var(--ok)}.player-choices{border-top:1px solid var(--line)}.player-choices label{border-bottom:1px solid var(--line);cursor:pointer;display:flex;gap:.8rem;padding:1rem .75rem}.player-choices label:has(input:checked){background:var(--panel-strong)}.player-choices input{accent-color:var(--accent);margin:.28rem 0 0}.player-choices strong,.player-choices small{display:block}.player-choices small,.instructions,details{color:var(--muted)}.player-choices em{color:var(--accent);font-size:.75rem;font-style:normal;font-weight:400}.notice{background:var(--panel-strong);border-radius:8px;color:var(--muted);margin:1rem 0;padding:.9rem 1rem}.notice strong{color:var(--text)}.command{align-items:center;background:var(--bg);border-radius:8px;display:flex;gap:.7rem;margin:.75rem 0;padding:.8rem}.command code{color:var(--accent);flex:1;font-size:.78rem;overflow-wrap:anywhere}.field-port{flex:0 1 7ch;max-width:8ch}.field-password{flex:0 1 18ch;max-width:20rem}.field-zone{max-width:20rem}.wizard-status{color:var(--ok);margin:1rem 0 0}.wizard-status.failed{color:var(--danger);font-family:var(--font-mono)}footer{border-top:1px solid var(--line);display:flex;justify-content:space-between;margin-top:1.3rem;padding-top:1rem}footer .primary-button{margin-left:auto}@media(max-width:720px){.wizard-layout{gap:1.25rem;grid-template-columns:1fr}.wizard-steps{font-size:.74rem;grid-template-columns:repeat(3,minmax(0,1fr))}.command{align-items:stretch;flex-direction:column}.field-zone{max-width:none}}
</style>
