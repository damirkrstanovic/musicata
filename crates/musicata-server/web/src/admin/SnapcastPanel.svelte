<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onMount } from "svelte";
  import { connectSnapcastHealth } from "../lib/snapcast-health";
  import type { SnapcastHealth } from "../types/SnapcastHealth";
  import { api, ApiError } from "../lib/api";
  import type { SnapcastStatus } from "../types/SnapcastStatus";
  import type { UsbWakePolicy } from "../types/UsbWakePolicy";
  import type { SnapClient } from "../types/SnapClient";
  import type { SnapRoomView } from "../types/SnapRoomView";
  import type { EqProfile } from "../lib/dsp";

  let status = $state<SnapcastStatus | null>(null);
  let rooms = $state<SnapRoomView[]>([]);
  let dspProfiles = $state<EqProfile[]>([]);
  let newRoom = $state("");
  let host = $state("");
  let bufferMs = $state(1000);
  let message = $state("");
  let error = $state(false);
  let busy = $state(false);
  let usbPolicy = $state<UsbWakePolicy | null>(null);
  let usbIdleSeconds = $state<number | undefined>(undefined);
  let health = $state<SnapcastHealth | null>(null);
  let healthConnected = $state(false);
  let loadError = $state("");
  let unavailable = $state(false);

  async function load() {
    try {
      status = await api.snapcastStatus();
      usbPolicy = await api.usbWakePolicy();
      usbIdleSeconds = usbPolicy.idle_seconds;
      health = status.health;
      loadError = "";
      host = status.server_host;
      bufferMs = status.buffer_ms;
      rooms = await api.snapcastRooms();
      dspProfiles = await api.dspProfiles().catch(() => []);
    } catch (e) {
      unavailable = e instanceof ApiError && e.status === 404;
      loadError = unavailable ? "" : "Multi-room status is unavailable. Check the server connection and try again.";
    }
  }
  onMount(() => {
    const disconnect = connectSnapcastHealth(value => {
      health = value;
      healthConnected = true;
      rooms = rooms.map(room => ({ ...room, connected: value.control === "connected" && value.clients.some(client => client.id === room.name && client.connected) }));
      if (status) {
        status.clients = value.clients;
        status.groups = value.groups;
        status.inputs = value.streams;
        status.running = value.control === "connected" && ["running", "external"].includes(value.process);
      }
    }, () => { healthConnected = false; });
    void load().then(() => { if (unavailable) disconnect(); });
    return disconnect;
  });

  async function retryPlayback() {
    busy = true;
    try { await api.retrySnapcast(); message = "Recovery requested."; error = false; }
    catch (e) { message = e instanceof Error ? e.message : String(e); error = true; }
    finally { busy = false; }
  }

  async function saveUsbPolicy(enabled: boolean) {
    if (usbIdleSeconds === undefined) return;
    busy = true;
    error = false;
    try {
      usbPolicy = await api.updateUsbWakePolicy({ enabled, idle_seconds: usbIdleSeconds }) ?? usbPolicy;
      message = "Saved. Configured players apply USB settings within 15 seconds.";
    } catch (e) {
      message = e instanceof ApiError ? e.message : String(e);
      error = true;
    } finally {
      busy = false;
    }
  }

  async function update(
    patch: {
      mpd_enabled?: boolean;
      buffer_ms?: number;
      enabled?: boolean;
      auth_enabled?: boolean;
      server_host?: string;
      airplay_enabled?: boolean;
      spotify_enabled?: boolean;
      route_all_input?: string;
      dsp_profile_id?: string;
    },
    note: string,
  ) {
    busy = true;
    message = "Saving…";
    error = false;
    try {
      const next = await api.updateSnapcast(patch);
      if (next) {
        status = next;
        host = next.server_host;
        bufferMs = next.buffer_ms;
      }
      if (status?.running) rooms = await api.snapcastRooms();
      message = note;
    } catch (e) {
      message = e instanceof ApiError ? e.message : String(e);
      error = true;
      status = await api.snapcastStatus().catch(() => null);
    } finally {
      busy = false;
    }
  }

  async function addRoom() {
    const name = newRoom.trim();
    if (!name) return;
    busy = true;
    error = false;
    try {
      const next = await api.addSnapcastRoom(name);
      if (next) rooms = next;
      newRoom = "";
      message = `Added “${name}”.`;
    } catch (e) {
      message = e instanceof ApiError ? e.message : String(e);
      error = true;
    } finally {
      busy = false;
    }
  }

  async function removeRoom(name: string) {
    busy = true;
    try {
      const next = await api.deleteSnapcastRoom(name);
      if (next) rooms = next;
      message = `Removed “${name}”.`;
    } catch (e) {
      message = e instanceof ApiError ? e.message : String(e);
      error = true;
    } finally {
      busy = false;
    }
  }

  async function setVolume(client: SnapClient, percent: number) {
    try {
      await api.setSnapcastVolume(client.id, percent);
    } catch {
      load();
    }
  }
</script>

{#if loadError && !unavailable}
  <section class="admin-panel" aria-label="Multi-room connection">
    <p role="status">{loadError}</p>
    <button class="ghost-button" onclick={load}>Retry connection</button>
  </section>
{/if}
{#if status}
  <section class="admin-panel">
    <div class="admin-panel-head"><h2>Multi-room (Snapcast)</h2></div>
    <p class="admin-hint">
      Play the same music perfectly in sync across rooms. Each room is a snapclient on the
      network; Musicata decodes and streams to them through a managed snapserver.
    </p>

    {#if health}
      <div data-snapcast-health>
        <h3 class="admin-subhead">Playback health</h3>
        <p class="admin-hint" role="status">
          {#if !healthConnected}Live updates disconnected; observations may be stale.
          {:else if health.failure}{health.failure}
          {:else if health.process === "disabled"}Multi-room playback is disabled.
          {:else}Control connection: {health.control}.{/if}
        </p>
        <p class="admin-hint">
          Server process: {health.process}. Recovery: {health.recovery}.
          {#if health.attempts}Attempts: {health.attempts} of 3.{/if}
          {#if health.next_retry_at}Next attempt: {new Date(health.next_retry_at * 1000).toLocaleTimeString()}.{/if}
          {#if health.server_version}Snapserver {health.server_version}.{/if}
        </p>
        <p class="admin-hint">
          {#if health.last_success_at}Last successful observation: {new Date(health.last_success_at * 1000).toLocaleString()}.
          {:else}No successful observation yet.{/if}
          {#if health.control !== "connected"}Client and stream information may be stale.{/if}
        </p>
        {#each health.streams as stream (stream.id)}
          <p class="admin-hint" data-snapcast-stream>
            {stream.id}: {stream.status}{!healthConnected || health.control !== "connected" ? " (last observed; may be stale)" : ""}.
          </p>
        {/each}
        <p class="admin-hint" data-physical-audio-unknown>
          Physical audio: unknown. A connected client or playing stream cannot confirm sound from a DAC or speaker.
          If there is no sound, check the playback host's service, selected audio device, permissions and cable.
          A missing or busy device may require restarting the client after reconnecting it.
        </p>
        {#if health.recovery === "exhausted"}
          <button class="ghost-button" disabled={busy || !healthConnected} onclick={retryPlayback}>Retry playback recovery</button>
        {/if}
      </div>
    {/if}

    <label class="toggle-row">
      <input
        type="checkbox"
        checked={status.enabled}
        disabled={busy}
        onchange={(e) =>
          update(
            { enabled: (e.currentTarget as HTMLInputElement).checked },
            (e.currentTarget as HTMLInputElement).checked
              ? "Multi-room is on."
              : "Will turn off on next restart.",
          )}
      />
      <span>Enable synchronized multi-room playback</span>
    </label>

    {#if status.enabled && !status.running}
      <p class="admin-hint">Enabled, but a healthy control connection has not been confirmed. Check playback health above.</p>
    {/if}

    <label class="field">
      <span>Playback buffer (milliseconds)</span>
      <div class="host-row">
        <input data-snapcast-buffer type="number" min="100" max="5000" step="1" bind:value={bufferMs} disabled={busy || !status.manage_server} />
        <button class="ghost-button" disabled={busy || !status.manage_server || bufferMs === status.buffer_ms || !Number.isInteger(bufferMs) || bufferMs < 100 || bufferMs > 5000}
          onclick={() => update({ buffer_ms: bufferMs }, status?.running ? "Playback buffer applied." : "Saved. Applies when multi-room starts.")}>Apply buffer</button>
      </div>
    </label>
    <p class="admin-hint">
      Smaller buffers respond sooner but tolerate less network jitter. Default: 1,000 ms.
      Try 250 ms for local or wired playback; increase it if audio drops out.
      Applying briefly restarts the multi-room server and interrupts all its inputs.
      {#if !status.manage_server}Configure this on your external Snapserver.{:else if status.applied_buffer_ms !== null}Currently applied: {status.applied_buffer_ms} ms.{/if}
    </p>

    <!-- Access control: per-room passwords. NOTE: snapserver 0.35 doesn't enforce them yet. -->
    <div class="admin-subhead">Access</div>
    <label class="toggle-row">
      <input
        type="checkbox"
        checked={status.auth_enabled}
        disabled={busy}
        onchange={(e) =>
          update(
            { auth_enabled: (e.currentTarget as HTMLInputElement).checked },
            "Saved. Restart the server to write the new config.",
          )}
      />
      <span>Require a password per room</span>
    </label>
    {#if status.auth_enabled}
      <p class="admin-hint warn">
        ⚠️ Not enforced yet: snapserver {`0.35`} ignores these passwords (upstream is still
        finishing auth), so rooms remain open on the local network for now. The passwords are
        baked into each room's command and config, and become enforcing automatically when a
        future snapserver enables auth. Keep the server on a trusted network meanwhile.
      </p>
    {/if}
    <label class="field">
      <span>Server address rooms connect to</span>
      <div class="host-row">
        <input bind:value={host} placeholder="musicata.local" disabled={busy} />
        <button
          class="ghost-button"
          disabled={busy || host === status.server_host}
          onclick={() => update({ server_host: host }, "Saved server address.")}
        >Save</button>
      </div>
    </label>

    {#if usbPolicy}
      <div class="admin-subhead">USB DAC standby</div>
      <p class="admin-hint">
        Configured players can disconnect their DAC after playback stops and reconnect it for music.
        First use the player installer to verify and bind an isolated USB port on each player.
      </p>
      <label class="toggle-row">
        <input data-usb-wake-enabled type="checkbox" checked={usbPolicy.enabled} disabled={busy}
          onchange={(e) => saveUsbPolicy(e.currentTarget.checked)} />
        <span>Allow USB standby on configured players</span>
      </label>
      <label class="field">
        <span>Continuous idle before standby (seconds)</span>
        <div class="host-row">
          <input data-usb-wake-delay type="number" min="1" max="3600" step="1" bind:value={usbIdleSeconds} disabled={busy} />
          <button data-usb-wake-save class="ghost-button" disabled={busy || usbIdleSeconds === undefined || usbIdleSeconds === usbPolicy.idle_seconds}
            onclick={() => saveUsbPolicy(usbPolicy!.enabled)}>Save</button>
        </div>
      </label>
    {/if}

    <div class="admin-subhead">MPD compatibility input</div>
    <p class="admin-hint">Use the installer to route local MPD through Snapcast. Snapclient plays
      the audio and can wake its USB DAC. Select MPD as the input for the desired rooms.
      Changing this setting briefly reconnects the rooms.</p>
    <label class="toggle-row">
      <input data-mpd-input type="checkbox" checked={status.mpd_enabled}
        disabled={busy || !status.running || !status.manage_server}
        onchange={(e) => update({mpd_enabled: e.currentTarget.checked}, "MPD input updated.")} />
      <span>Accept local MPD audio</span>
    </label>

    <!-- Cast TO Musicata: snapserver spawns shairport-sync / librespot and exposes each as a
         stream rooms can be switched to. The phone controls playback; Musicata just routes it. -->
    <div class="admin-subhead">Cast to Musicata</div>
    <p class="admin-hint">
      Let a phone AirPlay or Spotify-Connect audio into Musicata and play it across your rooms.
      Musicata doesn't own the playlist — the phone controls it — but ships it to multi-room.
    </p>
    <label class="toggle-row">
      <input
        type="checkbox"
        checked={status.airplay_enabled}
        disabled={busy}
        onchange={(e) =>
          update(
            { airplay_enabled: (e.currentTarget as HTMLInputElement).checked },
            "Saved. Restart the server to apply the input change.",
          )}
      />
      <span>Accept AirPlay (any iPhone / iPad / Mac)</span>
    </label>
    {#if status.airplay_enabled && !status.airplay_installed}
      <p class="admin-hint warn">⚠️ <code>shairport-sync</code> isn't installed — install it for AirPlay to work.</p>
    {/if}
    <label class="toggle-row">
      <input
        type="checkbox"
        checked={status.spotify_enabled}
        disabled={busy}
        onchange={(e) =>
          update(
            { spotify_enabled: (e.currentTarget as HTMLInputElement).checked },
            "Saved. Restart the server to apply the input change.",
          )}
      />
      <span>Accept Spotify Connect (any device, incl. Android)</span>
    </label>
    {#if status.spotify_enabled && !status.spotify_installed}
      <p class="admin-hint warn">⚠️ <code>librespot</code> isn't installed — install it for Spotify Connect to work.</p>
    {/if}
    {#if (status.airplay_enabled || status.spotify_enabled) && !status.running}
      <p class="admin-hint">Restart the server to start receiving — the input streams are declared at startup.</p>
    {/if}

    {#if status.running && status.inputs.length > 1}
      <!-- Which input all rooms play. Applied live; only streams snapserver actually has appear. -->
      <div class="admin-subhead">Route all groups</div>
      <div class="input-picker">
        {#each status.inputs as input (input.id)}
          <button
            type="button"
            class="input-chip"
            disabled={busy}
            onclick={() => update({ route_all_input: input.id }, `Routed all groups to ${input.id}.`)}
          >
            <span class="input-name">{input.id === "Musicata" ? "Library" : input.id}</span>
            {#if input.now_playing}<span class="input-now">{input.now_playing}</span>{/if}
          </button>
        {/each}
      </div>
    {/if}

    <!-- Server-side correction: apply an EQ/room profile to the multi-room PCM, in-process. -->
    <div class="admin-subhead">Correction (EQ / room)</div>
    <p class="admin-hint">
      Apply an EQ or room-correction profile to the multi-room output, processed on the server
      (no extra software). Create profiles in the player's Equalizer.
    </p>
    <label class="field">
      <select
        value={status.dsp_profile_id ?? ""}
        disabled={busy}
        onchange={(e) =>
          update(
            { dsp_profile_id: (e.currentTarget as HTMLSelectElement).value },
            "Saved correction.",
          )}
      >
        <option value="">None</option>
        {#each dspProfiles as p (p.id)}
          <option value={p.id}>{p.name}</option>
        {/each}
      </select>
    </label>

    {#if status.enabled || rooms.length > 0 || status.clients.length > 0}
      <!-- Rooms: provision a device with a name → get its install command. -->
      <div class="admin-subhead">Rooms</div>
      <div class="host-row">
        <input
          bind:value={newRoom}
          placeholder="kitchen"
          disabled={busy}
          onkeydown={(e) => e.key === "Enter" && addRoom()}
        />
        <button class="primary-button" disabled={busy || !newRoom.trim()} onclick={addRoom}>
          Add room
        </button>
      </div>

      {#if rooms.length === 0}
        <p class="admin-hint">No rooms yet. Add one to get a snapclient install command.</p>
      {:else}
        <ul class="room-list">
          {#each rooms as room (room.name)}
            <li class="provision">
              <div class="provision-head">
                <span class="room-name">
                  {room.name}
                  <span class="dot" class:on={healthConnected && health?.control === "connected" && room.connected} title={!healthConnected || health?.control !== "connected" ? "connection unknown" : room.connected ? "connected" : "not connected"}></span>
                </span>
                <button class="ghost-button" disabled={busy} onclick={() => removeRoom(room.name)}>Remove</button>
              </div>
              <p class="admin-hint">Install snapclient on the device, then run:</p>
              <input class="cmd" readonly value={room.command} onclick={(e) => (e.currentTarget as HTMLInputElement).select()} />
            </li>
          {/each}
        </ul>
      {/if}

      <!-- Live per-room volume for connected snapclients. -->
      {#if status.clients.length > 0}
        <div class="admin-subhead">Volume</div>
        <ul class="room-list">
          {#each status.clients as client (client.id)}
            <li class="room-row">
              <span class="room-name" class:offline={!client.connected}>
                {client.name}{!healthConnected || health?.control !== "connected" ? " (last observed; connection unknown)" : client.connected ? "" : " (offline)"}
              </span>
              <input
                class="room-volume"
                type="range"
                min="0"
                max="100"
                value={client.volume_percent}
                disabled={!healthConnected || health?.control !== "connected" || !client.connected}
                oninput={(e) =>
                  setVolume(client, Number((e.currentTarget as HTMLInputElement).value))}
              />
              <span class="room-pct">{client.volume_percent}%</span>
            </li>
          {/each}
        </ul>
      {/if}
    {/if}

    <p class="form-status" class:error>{message}</p>
  </section>
{/if}

<style>
  .admin-subhead {
    margin: 1rem 0 0.4rem;
    font-size: 0.78rem;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    opacity: 0.6;
  }
  .host-row {
    display: flex;
    gap: 0.5rem;
  }
  .host-row input {
    flex: 1;
  }
  .admin-hint.warn {
    color: #e0a33e;
  }
  .room-list {
    list-style: none;
    margin: 0.6rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
  }
  .provision {
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 0.5rem;
    padding: 0.6rem 0.7rem;
  }
  .provision-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .cmd {
    width: 100%;
    font-family: ui-monospace, monospace;
    font-size: 0.72rem;
  }
  .dot {
    display: inline-block;
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.25);
    margin-left: 0.3rem;
  }
  .dot.on {
    background: #4caf50;
  }
  .room-row {
    display: grid;
    grid-template-columns: 1fr 8rem 2.6rem;
    align-items: center;
    gap: 0.6rem;
  }
  .room-name.offline {
    opacity: 0.5;
  }
  .room-volume {
    width: 100%;
  }
  .room-pct {
    text-align: right;
    font-variant-numeric: tabular-nums;
    opacity: 0.7;
  }
  .input-picker {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    margin-top: 0.4rem;
  }
  .input-chip {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    align-items: flex-start;
    padding: 0.45rem 0.7rem;
    border-radius: 0.5rem;
    border: 1px solid rgba(255, 255, 255, 0.12);
    background: transparent;
    color: inherit;
    cursor: pointer;
  }
  .input-name {
    font-weight: 600;
    font-size: 0.85rem;
  }
  .input-now {
    font-size: 0.72rem;
    opacity: 0.7;
  }
</style>
