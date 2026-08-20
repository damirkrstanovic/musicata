<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onDestroy } from "svelte";
  import { api, ApiError, type ExportStatus } from "../lib/api";

  let status = $state<ExportStatus>({ running: false, latest: null, error: null });
  let exportMsg = $state("");
  let importMsg = $state("");
  let importError = $state(false);
  let importing = $state(false);
  let poll: ReturnType<typeof setInterval> | undefined;

  async function refresh() {
    try {
      status = await api.exportStatus();
    } catch {
      // keep
    }
    // Own the poll lifecycle here so an export already running when the panel mounts
    // (a reload mid-export, or a second admin tab) starts polling too — not only when
    // this tab is the one that kicked it off.
    if (status.running && !poll) {
      poll = setInterval(refresh, 2000);
    } else if (!status.running && poll) {
      clearInterval(poll);
      poll = undefined;
    }
  }
  refresh();
  onDestroy(() => poll && clearInterval(poll));

  async function startExport() {
    exportMsg = "";
    try {
      const next = await api.startExport();
      if (next) status = next;
      if (status.running && !poll) poll = setInterval(refresh, 2000);
    } catch (e) {
      exportMsg = e instanceof ApiError ? e.message : String(e);
    }
  }

  function fmtSize(bytes: number): string {
    const mb = bytes / (1024 * 1024);
    return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${mb.toFixed(1)} MB`;
  }

  async function onImport(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    importing = true;
    importMsg = "Uploading & unpacking…";
    importError = false;
    try {
      await api.importLibrary(file);
      importMsg = "Imported. Restart Musicata to load the imported library.";
    } catch (e) {
      importMsg = e instanceof ApiError ? e.message : String(e);
      importError = true;
    } finally {
      importing = false;
      input.value = "";
    }
  }
</script>

<section class="admin-panel">
  <div class="admin-panel-head"><h2>Backup &amp; migrate</h2></div>
  <p class="admin-hint">
    A single archive of your library database + acquired artwork (not the music files). Use it
    to back up or move Musicata to another machine.
  </p>

  <div class="field-form">
    <h3>Export</h3>
    <div class="field-actions">
      <button class="primary-button" type="button" disabled={status.running} onclick={startExport}>
        {status.running ? "Building…" : "Create export"}
      </button>
      {#if status.latest}
        <a class="ghost-button" href="/api/library/export/download" download>
          Download ({fmtSize(status.latest.size_bytes)})
        </a>
      {/if}
      <span class="form-status" class:error={!!status.error || !!exportMsg}>
        {status.error ?? exportMsg}
      </span>
    </div>
    {#if status.running}
      <p class="admin-hint">Snapshotting the database and zipping artwork — this runs in the background.</p>
    {/if}
  </div>

  <div class="field-form">
    <h3>Import</h3>
    <p class="admin-hint">
      Replaces this machine's library with the uploaded archive. Applied on the next restart.
    </p>
    <div class="field-actions">
      <label class="primary-button" style="cursor:pointer">
        {importing ? "Importing…" : "Choose archive…"}
        <input type="file" accept=".zip" hidden disabled={importing} onchange={onImport} />
      </label>
      <span class="form-status" class:error={importError}>{importMsg}</span>
    </div>
  </div>
</section>
