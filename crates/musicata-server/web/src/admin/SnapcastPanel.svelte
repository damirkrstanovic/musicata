<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, ApiError } from "../lib/api";
  import type { SnapcastStatus } from "../types/SnapcastStatus";
  import type { SnapClient } from "../types/SnapClient";
  import type { SnapRoomView } from "../types/SnapRoomView";
  import type { EqProfile } from "../lib/dsp";

  let status = $state<SnapcastStatus | null>(null);
  let rooms = $state<SnapRoomView[]>([]);
  let dspProfiles = $state<EqProfile[]>([]);
  let newRoom = $state("");
  let host = $state("");
  let message = $state("");
  let error = $state(false);
  let busy = $state(false);

  async function load() {
    try {
      status = await api.snapcastStatus();
      host = status.server_host;
      if (status.running) rooms = await api.snapcastRooms();
      dspProfiles = await api.dspProfiles().catch(() => []);
    } catch {
      // Built without the snapcast feature, or unavailable — hide the panel.
      status = null;
    }
  }
  load();

  async function update(
    patch: {
      enabled?: boolean;
      auth_enabled?: boolean;
      server_host?: string;
      airplay_enabled?: boolean;
      spotify_enabled?: boolean;
      active_input?: string;
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
      }
      if (status?.running) rooms = await api.snapcastRooms();
      message = note;
    } catch (e) {
      message = e instanceof ApiError ? e.message : String(e);
      error = true;
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

{#if status}
  <section class="admin-panel">
    <div class="admin-panel-head"><h2>Multi-room (Snapcast)</h2></div>
    <p class="admin-hint">
      Play the same music perfectly in sync across rooms. Each room is a snapclient on the
      network; Musicata decodes and streams to them through a managed snapserver.
    </p>

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
      <p class="admin-hint">Enabled, but not running this session — restart the server to start it.</p>
    {/if}

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
      <div class="admin-subhead">Playing in all rooms</div>
      <div class="input-picker">
        {#each status.inputs as input (input.id)}
          <button
            type="button"
            class="input-chip"
            class:active={input.id === status.active_input}
            disabled={busy}
            onclick={() => update({ active_input: input.id }, `Switched rooms to ${input.id}.`)}
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

    {#if status.running}
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
                  <span class="dot" class:on={room.connected} title={room.connected ? "connected" : "not connected"}></span>
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
                {client.name}{client.connected ? "" : " (offline)"}
              </span>
              <input
                class="room-volume"
                type="range"
                min="0"
                max="100"
                value={client.volume_percent}
                disabled={!client.connected}
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
  .input-chip.active {
    border-color: #d4af37;
    background: rgba(212, 175, 55, 0.12);
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
