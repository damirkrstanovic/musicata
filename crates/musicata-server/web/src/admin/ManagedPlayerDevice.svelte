<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onDestroy } from "svelte";
  import { api, ApiError, type ManagedPlayerDevice, type ManagedPlayerView } from "../lib/api";
  import type { SnapClient } from "../types/SnapClient";
  import { confirmAction } from "../lib/modal";

  let { client, onchange }: { client: SnapClient; onchange: () => Promise<void> } = $props();
  let open = $state(false);
  let view = $state<ManagedPlayerView | null>(null);
  let selected = $state<ManagedPlayerDevice | null>(null);
  let pairing = $state<{ token: string; expiresAt: number } | null>(null);
  let busy = $state(false);
  let message = $state("");
  let failed = $state(false);
  let poll: ReturnType<typeof setInterval> | undefined;
  let remainingPolls = 0;

  function errorText(error: unknown) {
    return error instanceof ApiError ? error.message : "Device management could not be updated. Try again.";
  }
  function pending(next = view) {
    return !!next?.report && next.desired.revision > Math.max(next.report.applied_revision, next.report.attempted_revision);
  }
  function stopPolling() {
    clearInterval(poll);
    poll = undefined;
    remainingPolls = 0;
  }
  async function refresh() {
    busy = true;
    failed = false;
    try {
      const next = await api.managedPlayer(client.id);
      view = next;
      selected = next.report?.devices.find(device => device.id === (next.desired.device_id ?? next.report?.selected_device_id)) ?? null;
      if (pending(next) && remainingPolls > 0) message = "Waiting for the player to apply the selected device…";
      else if (next.stale) message = "This player has not reported recently. A sleeping DAC does not by itself mean the player is offline.";
      else if (next.report?.status === "failed") message = next.report.error ?? "The player could not apply the requested audio device.";
      else if (next.report?.status === "unsupported") message = next.report.error ?? "This player does not support audio-device selection.";
      else message = "";
      if (!pending(next)) stopPolling();
    } catch (error) {
      message = errorText(error);
      failed = true;
      stopPolling();
    } finally {
      busy = false;
    }
  }
  function beginPolling() {
    stopPolling();
    remainingPolls = 12;
    poll = setInterval(() => {
      if (remainingPolls-- <= 0) {
        stopPolling();
        return;
      }
      if (!busy) void refresh();
    }, 2_000);
  }
  async function enable() {
    busy = true;
    failed = false;
    try {
      const next = await api.createManagedPlayerPairing(client.id);
      if (!next) throw new Error("Missing pairing response");
      pairing = { token: next.pairing_token, expiresAt: next.expires_at };
      message = "Pairing code created. It expires in ten minutes and is shown only here.";
    } catch (error) {
      message = errorText(error);
      failed = true;
    } finally {
      busy = false;
    }
  }
  async function copyPairing() {
    if (!pairing) return;
    try {
      await navigator.clipboard.writeText(pairing.token);
      message = "Pairing code copied.";
      failed = false;
    } catch {
      message = "Select and copy the pairing code.";
      failed = false;
    }
  }
  async function apply() {
    if (!view?.report || !selected || view.stale || !view.report.selection_supported || !selected.available) return;
    busy = true;
    failed = false;
    try {
      view = await api.saveManagedPlayerDevice(client.id, {
        device_id: selected.id,
        inventory_revision: view.report.inventory_revision,
      });
      message = "The player is changing audio device. It briefly restarts its client and does not start playback.";
      beginPolling();
    } catch (error) {
      message = errorText(error);
      failed = true;
    } finally {
      busy = false;
    }
  }
  async function revoke() {
    if (!(await confirmAction({title: "Disable device management", message: `Revoke device management for ${client.name}? The player keeps playing, but it can no longer report or change its audio device.`, confirmLabel: "Disable"}))) return;
    busy = true;
    try {
      await api.revokeManagedPlayer(client.id);
      view = null;
      selected = null;
      pairing = null;
      stopPolling();
      message = "Device management disabled.";
      failed = false;
      await onchange();
    } catch (error) {
      message = errorText(error);
      failed = true;
    } finally {
      busy = false;
    }
  }
  function openPanel() {
    open = true;
    pairing = null;
    void refresh();
  }
  function statusLabel() {
    if (!view?.paired) return "Not paired";
    if (view.stale) return "Reporting delayed";
    if (pending()) return "Applying";
    return view.report?.status === "applied" ? "Applied" : view.report?.status === "failed" ? "Needs attention" : "Paired";
  }
  function seenLabel() {
    if (!view?.last_seen) return "No report yet";
    return `Last report ${new Date(view.last_seen * 1_000).toLocaleString()}`;
  }
  onDestroy(stopPolling);
</script>

<details class="managed-device" bind:open ontoggle={(event) => { if ((event.currentTarget as HTMLDetailsElement).open) openPanel(); else { open = false; stopPolling(); } }}>
  <summary>Audio device</summary>
  {#if open}
    <p class="admin-hint">Musicata can manage a paired player’s reported audio devices. A sleeping DAC does not prove that the player is offline.</p>
    {#if !view?.paired}
      <button type="button" class="ghost-button" disabled={busy} onclick={() => void refresh()}>Refresh</button>
      <button type="button" class="ghost-button" disabled={busy} onclick={() => void enable()}>{busy ? "Preparing…" : "Enable device management"}</button>
      {#if pairing}
        <section class="pairing-instructions" aria-label="Player pairing instructions">
          <strong>Pair this managed player</strong>
          <p>For a player installed with the Musicata player installer, run this command on its host, then paste the code at the private prompt. Existing custom playback services must be reconciled locally first.</p>
          <div class="pairing-code"><code>{pairing.token}</code><button type="button" class="ghost-button" onclick={() => void copyPairing()}>Copy</button></div>
          <code>sudo scripts/install-player.sh --pair-code --musicata-url {location.origin}</code>
          <p class="admin-hint">Expires {new Date(pairing.expiresAt * 1_000).toLocaleTimeString()}.</p>
        </section>
      {/if}
    {:else}
      <div class="managed-status"><strong>{statusLabel()}</strong><span>{seenLabel()}</span></div>
      <div class="field-actions"><button type="button" class="ghost-button" disabled={busy} onclick={() => void refresh()}>{busy ? "Refreshing…" : "Refresh"}</button><button type="button" class="ghost-button danger" disabled={busy} onclick={() => void revoke()}>Disable management</button></div>
      {#if view.report}
        {#if !view.report.selection_supported}<p class="managed-note">{view.report.error ?? "This player does not support audio-device selection."}</p>{:else if view.stale}<p class="managed-note">Refresh after the player reports a current device inventory.</p>{:else}
          <fieldset disabled={busy || pending()}>
            <legend>Reported available devices</legend>
            <div class="managed-options">
              {#each view.report.devices as device (device.id)}
                <label class:disabled={!device.available}><input type="radio" name="managed-device-{client.id}" checked={selected?.id === device.id} disabled={!device.available} onchange={() => (selected = device)} /><span><strong>{device.name}</strong><small>{device.available ? "Available" : "Unavailable"}</small></span></label>
              {/each}
            </div>
          </fieldset>
          <button type="button" class="primary-button" disabled={busy || pending() || !selected?.available} onclick={() => void apply()}>Apply audio device</button>
        {/if}
      {:else}<p class="managed-note">Waiting for this paired player’s first device report.</p>{/if}
    {/if}
    {#if message}<p class="managed-message" class:failed role={failed ? "alert" : "status"}>{message}</p>{/if}
  {/if}
</details>

<style>
  .managed-device{flex-basis:100%;margin:.2rem 0}.managed-device summary{color:var(--muted);cursor:pointer;font-size:.82rem}.managed-device .admin-hint{margin:.55rem 0}.managed-status{display:grid;gap:.15rem;margin:.65rem 0}.managed-status span,.managed-note{color:var(--muted);font-size:.8rem}.pairing-instructions{background:var(--panel-strong);border-radius:8px;display:grid;gap:.55rem;margin-top:.7rem;padding:.85rem}.pairing-instructions p{color:var(--muted);font-size:.82rem;margin:0}.pairing-instructions>code{font-family:var(--font-mono);font-size:.75rem;overflow-wrap:anywhere}.pairing-code{align-items:center;background:var(--bg);border-radius:6px;display:flex;gap:.65rem;padding:.65rem}.pairing-code code{color:var(--accent);flex:1;overflow-wrap:anywhere}.managed-device fieldset{border:0;margin:.7rem 0;padding:0}.managed-device legend{color:var(--muted);font-size:.75rem}.managed-options{border-top:1px solid var(--line);display:grid}.managed-options label{align-items:center;border-bottom:1px solid var(--line);display:flex;gap:.6rem;padding:.55rem 0}.managed-options label.disabled{color:var(--muted)}.managed-options strong,.managed-options small{display:block}.managed-options small{color:var(--muted);font-size:.75rem}.managed-message{color:var(--ok);font-size:.82rem}.managed-message.failed{color:var(--danger);font-family:var(--font-mono)}
</style>
