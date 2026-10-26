<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, ApiError, type MpdOutput, type MpdOutputsView, type Player } from "../lib/api";

  let { player }: { player: Player } = $props();
  let view = $state<MpdOutputsView | null>(null);
  let chosen = $state<MpdOutput | null>(null);
  let busy = $state(false);
  let message = $state("");
  let failed = $state(false);
  let loadedAddress = $state<string | null>(null);

  $effect(() => {
    if (player.address === loadedAddress) return;
    const wasLoaded = loadedAddress !== null;
    loadedAddress = player.address;
    view = null;
    chosen = null;
    message = wasLoaded ? "Connection changed. Refresh configured MPD outputs for the new address." : "";
    failed = false;
  });

  function duplicate(output: MpdOutput) {
    return (view?.outputs.filter(candidate => candidate.name === output.name && candidate.plugin === output.plugin).length ?? 0) > 1;
  }

  async function refresh() {
    const address = player.address;
    busy = true;
    message = "Refreshing configured MPD outputs…";
    failed = false;
    try {
      const next = await api.playerOutputs(player.id);
      if (address !== player.address) return;
      if (!next) throw new Error("Missing output state");
      view = next;
      chosen = next.outputs.find(output => output.name === next.selected?.name && output.plugin === next.selected?.plugin) ?? null;
      message = !next.selection_supported
        ? (next.reason ?? "Output selection is unavailable for this MPD player.")
        : next.outputs.length
          ? "Configured MPD outputs refreshed. Enabled does not confirm that sound is audible."
          : "MPD did not report any configured outputs.";
    } catch (error) {
      message = error instanceof ApiError ? error.message : "Configured MPD outputs could not be refreshed.";
      failed = true;
    } finally {
      busy = false;
    }
  }

  async function apply() {
    if (!chosen || duplicate(chosen) || !view?.selection_supported) return;
    const address = player.address;
    busy = true;
    message = "Applying audio device…";
    failed = false;
    try {
      const next = await api.savePlayerOutput(player.id, { name: chosen.name, plugin: chosen.plugin });
      if (address !== player.address) return;
      if (!next) throw new Error("Missing output state");
      view = next;
      chosen = next.outputs.find(output => output.name === next.selected?.name && output.plugin === next.selected?.plugin) ?? null;
      message = "Audio device saved. Musicata does not start playback or verify audible sound.";
    } catch (error) {
      message = error instanceof ApiError ? error.message : "The audio device could not be saved.";
      failed = true;
    } finally {
      busy = false;
    }
  }
</script>

<details class="audio-device">
  <summary>Audio device</summary>
  <p class="admin-hint">Choose among outputs configured in MPD. This does not enumerate all hardware devices. Applying one enables it and disables the other configured outputs.</p>
  <div class="field-actions"><button type="button" class="ghost-button" disabled={busy} onclick={() => void refresh()}>{busy ? "Refreshing…" : "Refresh outputs"}</button></div>
  {#if view}
    {#if !view.selection_supported}<p class="audio-note" role="status">{view.reason ?? "Output selection is unavailable for this MPD player."}</p>{/if}
    {#if view.outputs.length}
      <fieldset disabled={busy || !view.selection_supported}>
        <legend>Configured MPD outputs</legend>
        <div class="output-options">
          {#each view.outputs as output (output.id)}
            {@const isDuplicate = duplicate(output)}
            <label class:disabled={isDuplicate}><input type="radio" name="mpd-output-{player.id}" checked={chosen?.id === output.id} disabled={isDuplicate} onchange={() => (chosen = output)} /><span><strong>{output.name}</strong><small>{output.plugin} · {output.enabled ? "Enabled" : "Disabled"}{isDuplicate ? " · Ambiguous duplicate" : ""}</small></span></label>
          {/each}
        </div>
      </fieldset>
      <button type="button" class="primary-button" disabled={busy || !chosen || duplicate(chosen) || !view.selection_supported} onclick={() => void apply()}>{busy ? "Applying…" : "Apply audio device"}</button>
    {/if}
  {/if}
  {#if message}<p class="audio-status" class:failed role={failed ? "alert" : "status"}>{message}</p>{/if}
</details>

<style>
  .audio-device{flex-basis:100%;margin:.2rem 0}.audio-device summary{color:var(--muted);cursor:pointer;font-size:.82rem}.audio-device .admin-hint{margin:.55rem 0}.audio-note{color:var(--muted);font-size:.82rem}.audio-device fieldset{border:0;margin:.6rem 0;padding:0}.audio-device legend{color:var(--muted);font-size:.75rem}.output-options{border-top:1px solid var(--line);display:grid}.output-options label{align-items:center;border-bottom:1px solid var(--line);display:flex;gap:.6rem;padding:.55rem 0}.output-options label.disabled{color:var(--muted)}.output-options strong,.output-options small{display:block}.output-options small{color:var(--muted);font-size:.75rem}.audio-status{color:var(--ok);font-size:.82rem}.audio-status.failed{color:var(--danger);font-family:var(--font-mono)}
</style>
