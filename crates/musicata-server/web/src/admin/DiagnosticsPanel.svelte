<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onMount } from "svelte";
  import { diagnosticStatus, setDiagnosticDetail, prepareDiagnostics, captureProfile, setStartupProfiles, type DiagnosticStatus } from "../lib/api";
  let status = $state<DiagnosticStatus | null>(null);
  let description = $state("");
  let problemTime = $state("");
  let message = $state("");
  let busy = $state(false);
  let includeProfiles = $state(false);
  let captureSeconds = $state(60);
  const memory = (bytes: number | null | undefined) => bytes == null ? "Unavailable" : `${(bytes / 1048576).toFixed(1)} MiB`;
  const time = (seconds: number | null) => seconds == null ? "None this session" : new Date(seconds * 1000).toLocaleTimeString();
  async function capture(kind: "cpu" | "heap") {
    busy = true; message = "";
    try { status = await captureProfile(kind, captureSeconds); }
    catch (error) { message = error instanceof Error ? error.message : "Could not start capture."; }
    finally { busy = false; }
  }
  async function startup(kind: "cpu" | "heap", enabled: boolean) {
    if (!status) return;
    busy = true; message = "";
    try { status = await setStartupProfiles(kind === "cpu" ? enabled : status.cpu_on_startup, kind === "heap" ? enabled : status.heap_on_startup); }
    catch (error) { message = error instanceof Error ? error.message : "Could not save startup capture."; }
    finally { busy = false; }
  }
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
      const next = await prepareDiagnostics(description, problemTime, includeProfiles);
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
    <div class="runtime-health" data-runtime-health>
      <h3>Resource usage</h3>
      {#if status.runtime.latest}
        {@const sample = status.runtime.latest}
        <dl class="resource-grid">
          <div><dt>CPU</dt><dd>{sample.cpu_percent == null ? "Measuring…" : `${sample.cpu_percent.toFixed(1)}%`}</dd></div>
          <div><dt>Process memory</dt><dd>{memory(sample.rss_bytes)}</dd></div>
          <div><dt>Swap</dt><dd>{memory(sample.swap_bytes)}</dd></div>
          <div><dt>Live allocations</dt><dd>{memory(sample.allocated_bytes)}</dd></div>
          <div><dt>Allocator resident memory</dt><dd>{memory(sample.allocator_resident_bytes)}</dd></div>
          <div><dt>Container memory</dt><dd>{memory(sample.container_bytes)}</dd></div>
        </dl>
        <p class="admin-hint">Sampled {time(sample.at)}. CPU 100% means one core. High process memory alone does not prove a leak. Resource history covers up to 24 hours.</p>
      {:else}<p class="admin-hint">Waiting for the first resource sample…</p>{/if}
      <h3>Background workers</h3>
      <p class="admin-hint">Counters cover this server session. Queue counts include unfinished work even when its worker is disabled. Failed attempts that can be retried are separate from worker restarts.</p>
      <div class="worker-table"><table>
        <thead><tr><th>Worker</th><th>State</th><th>Queued / active</th><th>Completed</th><th>Retryable failures</th><th>Restarts</th><th>Progress</th></tr></thead>
        <tbody>{#each status.runtime.workers as worker (worker.kind)}
          <tr><th>{worker.kind}</th><td>{worker.stalled ? "No progress — investigate" : worker.state}{#if worker.last_failure}<small>{worker.last_failure}</small>{/if}</td><td>{worker.queued ?? "—"} / {worker.active}</td><td>{worker.completed} <small>{worker.work_unit}</small></td><td>{worker.retryable_failures}{#if worker.next_retry_at}<small>Retry {time(worker.next_retry_at)}</small>{/if}</td><td>{worker.restarts}</td><td>{time(worker.last_progress_at)}<small>{(worker.completed * 60 / Math.max(1, status.runtime.latest?.uptime_seconds ?? 1)).toFixed(1)} / min this session</small>{#if worker.operation_started_at}<small>Active for {Math.max(0, Math.floor(Date.now()/1000-worker.operation_started_at))}s</small>{/if}</td></tr>
        {/each}</tbody>
      </table></div>
      {#if status.runtime.checkpoint_error}<p class="diagnostic-error">Resource history could not be saved. Current-session measurements remain available.</p>{/if}
      <h3>Performance profiles</h3>
      <p class="admin-hint">Capture CPU call stacks or sampled live allocations for a limited time. Captures stop automatically and stay on this server. Heap profiles cover allocations made during the capture; they do not explain older allocations.</p>
      {#if status.profiles.supported}
        <label>Capture duration <select bind:value={captureSeconds} disabled={busy || !!status.profiles.running}><option value={5}>5 seconds</option><option value={15}>15 seconds</option><option value={30}>30 seconds</option><option value={60}>60 seconds</option></select></label>
        <div class="field-actions">
          <button class="ghost-button" disabled={busy || !!status.profiles.running} onclick={() => capture("cpu")}>Capture CPU for {captureSeconds} seconds</button>
          <button class="ghost-button" disabled={busy || !!status.profiles.running || !status.profiles.heap_supported} onclick={() => capture("heap")}>Capture memory for {captureSeconds} seconds</button>
        </div>
        {#if status.profiles.running}<p data-profile-running>Capturing {status.profiles.running}; ends {time(status.profiles.ends_at)}.</p>{/if}
        <div class="field-actions">
          {#if status.profiles.cpu_ready}<a class="ghost-button" href="/api/diagnostics/profile/cpu" download="cpu-profile.svg">Download CPU flame graph</a>{/if}
          {#if status.profiles.heap_ready}<a class="ghost-button" href="/api/diagnostics/profile/heap" download="heap-profile.json">Download memory profile</a>{/if}
        </div>
        <label><input type="checkbox" checked={status.cpu_on_startup} disabled={busy} onchange={event => startup("cpu", event.currentTarget.checked)} /> Capture CPU for 60 seconds on startup</label>
        <label><input type="checkbox" checked={status.heap_on_startup} disabled={busy || !status.profiles.heap_supported} onchange={event => startup("heap", event.currentTarget.checked)} /> Capture memory for 60 seconds on startup</label>
        {#if !status.profiles.heap_supported}<p class="admin-hint">Heap stack capture is unavailable in this build. The Linux Docker build supports it; allocator statistics are still shown here.</p>{/if}
        <p class="admin-hint">Startup choices take effect on the next restart. If both are enabled, the captures run consecutively.</p>
      {:else}<p class="admin-hint">Stack profiling is unavailable in this build. The Linux Docker build supports CPU and heap captures. Resource and worker diagnostics remain available.</p>{/if}
      {#if status.profiles.error}<p class="diagnostic-error">{status.profiles.error}</p>{/if}
    </div>
    <div class="field-form">
      <label class="field description"><span>What happened? <em>(optional)</em></span><textarea bind:value={description} maxlength="1000" rows="3" placeholder="Playback stopped during the next track…"></textarea></label>
      <label class="field"><span>Approximate problem time <em>(optional)</em></span><input type="datetime-local" bind:value={problemTime} /></label>
      <p class="admin-hint">Your description is included in the download. Please leave out passwords and other private details.</p>
      <label><input type="checkbox" bind:checked={includeProfiles} /> Include captured performance profiles in the download</label>
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
  .runtime-health { display: grid; gap: 0.75rem; margin-block: 1.5rem; }
  .resource-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 1rem; margin: 0; }
  .resource-grid dt { font-size: 0.85rem; opacity: 0.75; }
  .resource-grid dd { margin: 0.25rem 0 0; font-family: var(--font-mono); }
  .worker-table { overflow-x: auto; }
  table { width: 100%; border-collapse: collapse; text-align: left; }
  th, td { padding: 0.6rem; vertical-align: top; }
  small { display: block; opacity: 0.75; }
  .description { max-width: 48ch; }
  textarea { width: 100%; box-sizing: border-box; resize: vertical; }
  input { max-width: 24ch; }
  .diagnostic-error { color: var(--danger); font-family: var(--font-mono); }
</style>
