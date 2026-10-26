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
  let snapcastHost = $state("");
  let port = $state(6600);
  let password = $state("");
  let zone = $state("");
  let newZone = $state("");
  let snapcast = $state<SnapcastStatus | null>(null);
  let selectedSnapcastId = $state("");
  let savedSnapcastName = $state<string | null>(null);
  let savedSnapcastZoneId = $state<string | null>(null);
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
  const snapcastServerHost = $derived(snapcastHost.trim() || serverHost);
  const snapcastCandidates = $derived(
    (snapcast?.health.control === "connected" ? snapcast.clients : []).filter(
      client => client.connected && !client.zone_id,
    ),
  );
  const selectedSnapcast = $derived(snapcastCandidates.find(client => client.id === selectedSnapcastId) ?? null);
  const continueEnabled = $derived(kind === "browser" || (kind === "mpd" ? checked : !!selectedSnapcast));
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
    snapcastHost = "";
    port = 6600;
    password = "";
    zone = "";
    newZone = "";
    selectedSnapcastId = "";
    savedSnapcastName = null;
    savedSnapcastZoneId = null;
    mpdAddress = "";
    checked = false;
    createdZoneId = null;
    createdPlayerId = null;
    message = "";
    failed = false;
    void focusTitle();
  }

  function cancel() {
    open = false;
  }

  function choose(next: Kind) {
    kind = next;
    checked = false;
    message = "";
    failed = false;
    if (next !== "snapcast") {
      selectedSnapcastId = "";
      savedSnapcastName = null;
      savedSnapcastZoneId = null;
    }
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
      if (kind === "snapcast") await refreshSnapcast();
      await focusTitle();
      return;
    }
    if (continueEnabled) {
      if (kind === "snapcast" && !(await refreshSnapcast())) return;
      step = 2;
      if (!name) name = kind === "mpd" ? "MPD player" : kind === "snapcast" ? selectedSnapcast?.name || "Snapcast speaker" : "This browser";
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

  async function discoverSnapcast() {
    try {
      const current = await api.snapcastStatus();
      snapcast = current;
      if (current.health.control !== "connected") {
        throw new Error("Snapcast discovery is unavailable. Check that the Snapserver control connection is running, then refresh.");
      }
      const candidates = current.clients.filter(client => client.connected && !client.zone_id);
      if (selectedSnapcastId && !candidates.some(client => client.id === selectedSnapcastId)) {
        throw new Error("The selected player is no longer connected or is already assigned. Refresh players and choose one explicitly.");
      }
      message = candidates.length
        ? "Choose the connected player you are setting up. Players already assigned to a zone are managed in Zones & players."
        : "No connected, unassigned Snapclients were found. Install Snapclient, then refresh players.";
      return true;
    } catch (e) {
      message = errorText(e);
      failed = true;
      return false;
    }
  }

  async function refreshSnapcast() {
    busy = true;
    message = "Refreshing connected players…";
    failed = false;
    try {
      return await discoverSnapcast();
    } finally {
      busy = false;
    }
  }

  function shellQuote(value: string) {
    return `'${value.replaceAll("'", "'\\''")}'`;
  }

  function chooseSnapcastClient(id: string) {
    if (id !== selectedSnapcastId) {
      selectedSnapcastId = id;
      savedSnapcastName = null;
      savedSnapcastZoneId = null;
    }
    message = "";
    failed = false;
  }

  async function copyCommand(command: string) {
    try {
      await navigator.clipboard.writeText(command);
      message = "Command copied.";
      failed = false;
    } catch {
      message = "Select and copy the command.";
      failed = false;
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
      if (kind === "snapcast") {
        if (!selectedSnapcastId) throw new Error("Choose a connected Snapclient before saving.");
        if (!(await discoverSnapcast())) return;
      }
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
        if (!selectedSnapcast) throw new Error("Choose a connected Snapclient before saving.");
        if (savedSnapcastName !== playerName) {
          await api.updateSnapcastClient(selectedSnapcast.id, { name: playerName });
          savedSnapcastName = playerName;
        }
        if (zoneId && savedSnapcastZoneId !== zoneId) {
          await api.updateSnapcastClient(selectedSnapcast.id, { zone_id: zoneId });
          savedSnapcastZoneId = zoneId;
        }
      }
      busy = false;
      open = false;
      void oncomplete().catch(() => {
        // The player was saved. A later panel refresh can retry without reopening the wizard.
      });
      return;
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
        <h4>What kind of player?</h4><p class="admin-hint">Choose how this device plays music. You can mix player types in a zone.</p><div class="player-choices" role="radiogroup" aria-label="Player type"><label><input data-player-kind="snapcast" type="radio" name="kind" checked={kind==="snapcast"} onchange={() => choose("snapcast")} /><span><strong>Snapcast speaker <em>Synchronized multi-room</em></strong><small>For a speaker connected to a device running Snapclient. Speakers in one zone play in sync.</small></span></label><label><input data-player-kind="mpd" type="radio" name="kind" checked={kind==="mpd"} onchange={() => choose("mpd")} /><span><strong>MPD player</strong><small>Connect an existing Music Player Daemon by hostname or IP address.</small></span></label><label><input data-player-kind="browser" type="radio" name="kind" checked={kind==="browser"} onchange={() => choose("browser")} /><span><strong>This browser</strong><small>Play through this device’s browser. No installation needed.</small></span></label></div>
      {:else if step===1}
        <h4>Connect</h4>
        {#if kind==="snapcast"}{#if !snapcast?.enabled}<p class="admin-hint">Multi-room playback is disabled. Enable it before discovering a speaker.</p><button type="button" class="primary-button" disabled={busy} onclick={() => void enable()}>Enable Snapcast</button>{:else}<p class="admin-hint">Install Snapclient on the player device, then choose it after it connects. Use this setup only on a trusted home or private network.</p><label class="field field-host"><span>Snapcast server host</span><input bind:value={snapcastHost} placeholder={serverHost} disabled={busy} /></label><ol class="instructions"><li>From a source checkout, run the installer on the player device.</li><li>From a release archive, use <code>./install-player.sh</code> in place of <code>scripts/install-player.sh</code>.</li><li>Return here and refresh connected players.</li></ol><div class="command"><code>sudo ./scripts/install-player.sh --server {shellQuote(snapcastServerHost)}</code><button type="button" class="ghost-button" onclick={() => void copyCommand(`sudo ./scripts/install-player.sh --server ${shellQuote(snapcastServerHost)}`)}>Copy command</button></div><button type="button" class="ghost-button" disabled={busy} onclick={() => void refreshSnapcast()}>Refresh players</button>{#if snapcastCandidates.length}<fieldset class="snapcast-candidates" disabled={busy}><legend>Connected players without a zone</legend>{#each snapcastCandidates as client (client.id)}<label><input type="radio" name="snapcast-client" value={client.id} checked={selectedSnapcastId===client.id} onchange={() => chooseSnapcastClient(client.id)} /><span><strong>{client.name || client.hostname}</strong><small>{client.hostname}{client.ip_address ? ` · ${client.ip_address}` : ""}</small><details><summary>Advanced identity</summary><code>{client.id}</code></details></span></label>{/each}</fieldset>{/if}<details><summary>Advanced manual setup</summary><p>If Snapclient is already installed, run <code>snapclient --host {shellQuote(snapcastServerHost)}</code>. This command has no Musicata credentials.</p></details>{/if}
        {:else if kind==="mpd"}<p class="admin-hint">Musicata connects to MPD already running on your device.</p><div class="field-grid"><label class="field field-host"><span>Hostname or IP address</span><input bind:value={host} placeholder="kitchen.local or 192.168.1.36" disabled={busy} oninput={() => checked=false} /></label><label class="field field-port"><span>Port</span><input type="number" min="1" max="65535" bind:value={port} disabled={busy} oninput={() => checked=false} /></label><label class="field field-password"><span>Password <em>(optional)</em></span><input type="password" bind:value={password} placeholder="Only if MPD requires one" disabled={busy} oninput={() => checked=false} /></label></div><button type="button" class="ghost-button" disabled={busy} onclick={() => void checkMpd()}>Test connection</button><details><summary>MPD is not installed yet?</summary><p>Install and configure MPD on the device first. For synchronized multi-room speakers, choose Snapcast.</p></details>
        {:else}<p class="admin-hint">Use the speakers or headphones attached to this device.</p><div class="notice"><strong>This browser is already a player</strong><br />Continue to give it a name and choose a zone. Keep the Musicata tab open while it plays.</div><p class="admin-hint">Browser playback follows zone controls, but is not sample-synchronized with Snapcast speakers.</p>{/if}
      {:else}
        <h4>Name &amp; zone</h4><p class="admin-hint">This is how the player will appear in Musicata.</p><label class="field field-name"><span>Player name</span><input bind:value={name} maxlength="64" placeholder="Kitchen speaker" /></label><label class="field field-zone"><span>Zone</span><select bind:value={zone} disabled={busy || !!createdZoneId}><option value="">{kind === "snapcast" ? "Shared playback — existing default queue" : "No zone — play independently"}</option>{#each zones as item (item.id)}<option value={item.id}>{item.name}</option>{/each}<option value="new">Create a new zone…</option></select></label>{#if zone==="new"}<label class="field field-name"><span>New zone name</span><input bind:value={newZone} disabled={busy || !!createdZoneId} maxlength="64" placeholder="Kitchen" /></label>{/if}<div class="notice"><strong>{zone ? "Control these players together" : kind === "snapcast" ? "Shared playback" : "An independent player"}</strong><br />{zone ? "The player will follow this zone’s queue, play/pause and track changes." : kind === "snapcast" ? "This speaker follows the existing default queue. Create a zone for independent playback." : "This player keeps its own queue. You can add it to a zone later."}</div>
        {#if kind === "snapcast" && zone}<p class="admin-hint">Adding the first speaker to a zone can briefly reconnect network players.</p>{/if}
      {/if}
      {#if message}<p class="wizard-status" class:failed role={failed?"alert":"status"}>{message}</p>{/if}<footer><button type="button" class="ghost-button" disabled={busy || step===0 || !!createdPlayerId || !!createdZoneId} onclick={() => {step-=1;void focusTitle();}}>Back</button>{#if step<2}<button type="button" class="primary-button" disabled={busy || (step===1 && !continueEnabled)} onclick={() => void next()}>Continue</button>{:else}<button type="button" class="primary-button" disabled={busy} onclick={() => void save()}>{busy ? "Saving…" : kind==="browser" ? "Save player" : "Add player"}</button>{/if}</footer>
    </div></div>
  </section>
{/if}

<style>
  .add-player-button{margin-top:1.2rem}.add-player-wizard{border-top:1px solid var(--line);margin-top:1.2rem;padding-top:1.2rem}.add-player-wizard header{display:flex;justify-content:space-between;gap:1rem}.add-player-wizard h3,.wizard-content h4{font-family:var(--font-display);font-size:1.35rem;font-weight:400;margin:0}.add-player-wizard header p{color:var(--muted);margin:.3rem 0 0}.wizard-layout{border-top:1px solid var(--line);display:grid;gap:2.5rem;grid-template-columns:11rem minmax(0,42rem);align-items:start;margin-top:1.25rem;padding-top:1.25rem}.wizard-steps{color:var(--muted);display:grid;gap:.85rem;list-style:none;margin:0;padding:0}.wizard-steps li{align-items:center;display:flex;gap:.55rem}.wizard-steps b{border:1px solid var(--line);border-radius:50%;display:grid;font-size:.75rem;height:1.65rem;place-items:center;width:1.65rem}.wizard-steps [aria-current="step"]{color:var(--text)}.wizard-steps [aria-current="step"] b{background:var(--accent);border-color:var(--accent);color:var(--bg)}.wizard-steps .done b{color:var(--ok)}.player-choices{border-top:1px solid var(--line)}.player-choices label,.snapcast-candidates label{border-bottom:1px solid var(--line);cursor:pointer;display:flex;gap:.8rem;padding:1rem .75rem}.player-choices label:has(input:checked),.snapcast-candidates label:has(input:checked){background:var(--panel-strong)}.player-choices input,.snapcast-candidates input{accent-color:var(--accent);margin:.28rem 0 0}.player-choices strong,.player-choices small,.snapcast-candidates strong,.snapcast-candidates small{display:block}.player-choices small,.snapcast-candidates small,.instructions,details{color:var(--muted)}.player-choices em{color:var(--accent);font-size:.75rem;font-style:normal;font-weight:400}.snapcast-candidates{border:0;border-top:1px solid var(--line);margin:1rem 0 0;padding:0}.snapcast-candidates legend{color:var(--muted);font-size:.8rem;padding:.6rem 0}.snapcast-candidates details{font-size:.78rem;margin-top:.35rem}.notice{background:var(--panel-strong);border-radius:8px;color:var(--muted);margin:1rem 0;padding:.9rem 1rem}.notice strong{color:var(--text)}.command{align-items:center;background:var(--bg);border-radius:8px;display:flex;gap:.7rem;margin:.75rem 0;padding:.8rem}.command code{color:var(--accent);flex:1;font-size:.78rem;overflow-wrap:anywhere}.field-port{flex:0 1 7ch;max-width:8ch}.field-password{flex:0 1 18ch;max-width:20rem}.field-zone{max-width:20rem}.wizard-status{color:var(--ok);margin:1rem 0 0}.wizard-status.failed{color:var(--danger);font-family:var(--font-mono)}footer{border-top:1px solid var(--line);display:flex;justify-content:space-between;margin-top:1.3rem;padding-top:1rem}footer .primary-button{margin-left:auto}@media(max-width:720px){.wizard-layout{gap:1.25rem;grid-template-columns:1fr}.wizard-steps{font-size:.74rem;grid-template-columns:repeat(3,minmax(0,1fr))}.command{align-items:stretch;flex-direction:column}.field-zone{max-width:none}}
</style>
