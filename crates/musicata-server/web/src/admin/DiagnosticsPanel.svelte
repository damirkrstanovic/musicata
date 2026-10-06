<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onMount } from "svelte";
  import { diagnosticStatus, setDiagnosticDetail, prepareDiagnostics, type DiagnosticStatus } from "../lib/api";
  let status = $state<DiagnosticStatus | null>(null);
  let description = $state("");
  let problemTime = $state("");
  let message = $state("");
  let busy = $state(false);
  async function refresh() {
    try { status = await diagnosticStatus(); }
    catch (error) { message = error instanceof Error ? error.message : "Could not load diagnostics."; }
  }
  onMount(() => {
    void refresh();
    const timer = setInterval(() => { void refresh(); }, 2000);
    return () => clearInterval(timer);
  });
  async function detail() {
    busy = true; message = "";
    try { status = await setDiagnosticDetail(!(status?.health.detail_remaining_seconds)); }
    catch (error) { message = error instanceof Error ? error.message : "Could not change recording."; }
    finally { busy = false; }
  }
  async function prepare() {
    busy = true; message = "";
    try {
      const next = await prepareDiagnostics(description, problemTime);
      if (status) status = { ...status, export: next };
    } catch (error) { message = error instanceof Error ? error.message : "Could not prepare diagnostics."; }
    finally { busy = false; }
  }
</script>

<section class="admin-panel" data-diagnostics>
  <div class="admin-panel-head"><h2>Diagnostics</h2></div>
  <p class="admin-hint">Musicata keeps a limited history of problems on this installation. Download it to help investigate a problem. Nothing is uploaded automatically.</p>
  {#if status}
    <p data-diagnostic-health>{status.health.available ? "Recording problems locally." : "Diagnostic storage is unavailable. Playback can continue; Musicata will retry."}</p>
    {#if status.health.dropped_events || status.health.storage_gaps || status.health.reported_lost_events}
      <p class="admin-hint">Some evidence could not be recorded. The download includes information about these gaps.</p>
    {/if}
    <div class="field-form">
      <label class="field description"><span>What happened? <em>(optional)</em></span><textarea bind:value={description} maxlength="1000" rows="3" placeholder="Playback stopped during the next track…"></textarea></label>
      <label class="field"><span>Approximate problem time <em>(optional)</em></span><input type="datetime-local" bind:value={problemTime} /></label>
      <p class="admin-hint">Your description is included in the download. Please leave out passwords and other private details.</p>
      <div class="field-actions">
        <button class="primary-button" type="button" disabled={busy || status.export.running} onclick={prepare}>{status.export.running ? "Preparing…" : "Prepare diagnostics"}</button>
        {#if status.export.ready}<a class="ghost-button" data-diagnostic-download href="/api/diagnostics/export/download" download="musicata-diagnostics.zip">Download diagnostics</a>{/if}
      </div>
      <div class="field-actions">
        <button class="ghost-button" type="button" disabled={busy} onclick={detail}>{status.health.detail_remaining_seconds ? "Stop detailed recording" : "Record more detail for 15 minutes"}</button>
        {#if status.health.detail_remaining_seconds}<span class="admin-hint" data-diagnostic-detail>Detailed recording ends in {Math.ceil(status.health.detail_remaining_seconds / 60)} minutes.</span>{/if}
      </div>
      {#if status.export.error}<p class="diagnostic-error">{status.export.error}</p>{/if}
    </div>
  {/if}
  {#if message}<p class="diagnostic-error">{message}</p>{/if}
</section>
<style>
  .description { max-width: 48ch; }
  textarea { width: 100%; box-sizing: border-box; resize: vertical; }
  input { max-width: 24ch; }
  .diagnostic-error { color: var(--danger); font-family: var(--font-mono); }
</style>
