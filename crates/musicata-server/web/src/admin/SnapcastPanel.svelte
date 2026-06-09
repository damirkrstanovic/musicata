<script lang="ts">
  import { api, ApiError } from "../lib/api";
  import type { SnapcastStatus } from "../types/SnapcastStatus";
  import type { SnapClient } from "../types/SnapClient";

  let status = $state<SnapcastStatus | null>(null);
  let message = $state("");
  let error = $state(false);
  let busy = $state(false);

  async function load() {
    try {
      status = await api.snapcastStatus();
    } catch {
      // Built without the snapcast feature, or the endpoint is unavailable — hide the panel.
      status = null;
    }
  }
  load();

  async function toggle(enabled: boolean) {
    busy = true;
    message = enabled ? "Starting…" : "Saving…";
    error = false;
    try {
      status = await api.setSnapcastEnabled(enabled);
      message = enabled
        ? "Multi-room is on. Point snapclients at this server."
        : "Will turn off on next restart.";
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
      // Non-fatal; refresh to show the server's actual value.
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
        onchange={(e) => toggle((e.currentTarget as HTMLInputElement).checked)}
      />
      <span>Enable synchronized multi-room playback</span>
    </label>

    {#if status.enabled && !status.running}
      <p class="admin-hint">Enabled, but not running this session — restart the server to start it.</p>
    {/if}

    {#if status.running}
      {#if status.clients.length === 0}
        <p class="admin-hint">No rooms connected yet. Start a snapclient on a device to add one.</p>
      {:else}
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
  .room-list {
    list-style: none;
    margin: 0.6rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
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
</style>
