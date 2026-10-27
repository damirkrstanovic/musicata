<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onDestroy } from "svelte";
  import { api, ApiError, type SourceView } from "../lib/api";
  import { connectActivity } from "../lib/activity";
  import { confirmAction } from "../lib/modal";

  let sources = $state<SourceView[]>([]);
  let status = $state("");
  let error = $state(false);
  let busy = $state(false);
  let editing = $state<string | null>(null);
  let editName = $state("");
  let editUsername = $state("");
  let editDomain = $state("");
  let editPassword = $state("");
  let clearPassword = $state(false);
  let updatingSourceId = $state<string | null>(null);
  let sourceRefreshFailed = $state(false);
  let kind = $state<"smb" | "opensubsonic" | "podcast" | "archive">("smb");

  const CAPS: [keyof SourceView["capabilities"], string][] = [
    ["can_scan", "Scan"],
    ["can_browse", "Browse"],
    ["can_search", "Search"],
    ["can_stream", "Stream"],
  ];

  async function load() {
    try {
      sources = await api.sources();
    } catch {}
  }
  load();

  const disconnectActivity = connectActivity((activities) => {
    if (updatingSourceId && activities.some((activity) => activity.kind === "source" && activity.status !== "running")) {
      void refreshUpdatedSource();
    }
  });
  onDestroy(disconnectActivity);

  function setStatus(message: string, isError = false) {
    status = message;
    error = isError;
  }

  async function addSource(event: SubmitEvent) {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const data = new FormData(form);
    const text = (name: string) => String(data.get(name) ?? "").trim();
    busy = true;
    setStatus("Connecting…");
    try {
      if (kind === "smb") {
        await api.createSource({
          kind: "smb",
          host: text("host"),
          share: text("share"),
          base_path: text("base_path") || undefined,
          display_name: text("display_name") || undefined,
          username: text("username") || undefined,
          password: text("password") || undefined,
        });
      } else if (kind === "opensubsonic") {
        await api.createSource({
          kind: "opensubsonic",
          host: text("host"),
          username: text("username") || undefined,
          password: text("password") || undefined,
          display_name: text("display_name") || undefined,
        });
      } else {
        // Podcast (feed URL) and Internet Archive (item id / details URL) both carry a single
        // location in `host`; episodes/files are streamed, not scanned.
        await api.createSource({
          kind,
          host: text("host"),
          display_name: text("display_name") || undefined,
        });
      }
      form.reset();
      setStatus("Added — scanning in the background.");
      await load();
    } catch (e) {
      setStatus(e instanceof ApiError ? e.message : String(e), true);
    } finally {
      busy = false;
    }
  }

  async function remove(source: SourceView) {
    if (
      !(await confirmAction({
        title: "Remove source",
        message: `Remove “${source.display_name}”? Its tracks leave the library.`,
        confirmLabel: "Remove",
      }))
    )
      return;
    try {
      await api.deleteSource(source.id);
      await load();
    } catch (e) {
      setStatus(e instanceof ApiError ? e.message : String(e), true);
    }
  }

  function edit(source: SourceView) {
    editing = source.id;
    editName = source.display_name;
    editUsername = source.username ?? "";
    editDomain = source.domain ?? "";
    editPassword = "";
    clearPassword = false;
    setStatus("");
  }

  async function saveSource(source: SourceView) {
    busy = true;
    setStatus("Checking the new settings…");
    try {
      const password = clearPassword ? null : editPassword === "" ? undefined : editPassword;
      sourceRefreshFailed = false;
      await api.updateSource(source.id, {
        display_name: editName,
        username: editUsername === "" ? null : editUsername,
        domain: editDomain === "" ? null : editDomain,
        ...(password === undefined ? {} : { password }),
      });
      editing = null;
      // An already-completed validation may have published before the activity subscription
      // sees it, so read once immediately after the accepted response.
      updatingSourceId = source.id;
      await refreshUpdatedSource();
    } catch (e) {
      updatingSourceId = null;
      sourceRefreshFailed = false;
      setStatus(e instanceof ApiError ? e.message : String(e), true);
    } finally {
      busy = false;
    }
  }

  async function refreshUpdatedSource() {
    const id = updatingSourceId;
    if (!id) return;
    try {
      const nextSources = await api.sources();
      sources = nextSources;
      sourceRefreshFailed = false;
      const source = nextSources.find((item) => item.id === id);
      if (!source) {
        setStatus("This source was removed before its settings could be saved.", true);
        updatingSourceId = null;
      } else if (source.update_status?.state === "failed") {
        setStatus(source.update_status.message ?? "Could not validate the changed source settings.", true);
        updatingSourceId = null;
      } else if (!source.update_status) {
        setStatus("Source settings saved.");
        updatingSourceId = null;
      }
    } catch {
      sourceRefreshFailed = true;
      setStatus("Still checking the new settings. The saved source configuration is unchanged until validation succeeds.");
    }
  }

  async function rescanAll() {
    try {
      await api.rescanAll();
      setStatus("Rescan started.");
    } catch (e) {
      setStatus(e instanceof ApiError ? e.message : String(e), true);
    }
  }
</script>

<section class="admin-panel">
  <div class="admin-panel-head">
    <h2>Music sources</h2>
    <button type="button" class="ghost-button" onclick={rescanAll}>Rescan all</button>
  </div>
  <p class="admin-hint">Your local library plus any network shares. New shares scan in the background.</p>

  <div class="admin-list">
    {#each sources as source (source.id)}
      <div class="admin-row">
        <div class="admin-row-main">
          <div class="admin-row-title">
            <strong>{source.display_name}</strong>
            <span class="tag">{source.kind}</span>
          </div>
          <div class="chips">
            {#each CAPS as [key, label] (key)}
              {#if source.capabilities[key]}<span class="chip">{label}</span>{/if}
            {/each}
          </div>
          {#if source.update_status}
            <p class="form-status" class:error={source.update_status.state === "failed"}>{source.update_status.message}</p>
          {/if}
        </div>
        {#if source.kind !== "local"}
          <div class="field-actions">
            <button type="button" class="ghost-button" disabled={busy || source.update_status?.state === "pending"} onclick={() => edit(source)}>Edit</button>
            <button type="button" class="ghost-button danger" disabled={busy || source.update_status?.state === "pending"} onclick={() => remove(source)}>Remove</button>
          </div>
        {/if}
      </div>
      {#if editing === source.id}
        <form class="field-form source-edit" onsubmit={(event) => { event.preventDefault(); void saveSource(source); }}>
          <h3>Edit {source.display_name}</h3>
          <div class="field-grid">
            <label class="field"><span>Name</span><input bind:value={editName} required disabled={busy} /></label>
            <label class="field"><span>User <em>(optional)</em></span><input bind:value={editUsername} disabled={busy} /></label>
            <label class="field"><span>Domain <em>(optional)</em></span><input bind:value={editDomain} disabled={busy} /></label>
            <label class="field"><span>New password</span><input bind:value={editPassword} type="password" autocomplete="new-password" disabled={busy || clearPassword} placeholder={source.password_configured ? "Saved — leave blank to keep" : "Not set"} /></label>
            <label class="field checkbox"><input type="checkbox" bind:checked={clearPassword} disabled={busy || !source.password_configured} /><span>Remove saved password</span></label>
          </div>
          <p class="admin-hint">The source type and location stay fixed. To change them, remove this source and add the new location.</p>
          <div class="field-grid readonly-source">
            <label class="field"><span>Type</span><input value={source.kind} readonly /></label>
            {#if source.host}<label class="field"><span>{source.kind === "opensubsonic" ? "Server URL" : "Host"}</span><input value={source.host} readonly /></label>{/if}
            {#if source.share}<label class="field"><span>Share</span><input value={source.share} readonly /></label>{/if}
            {#if source.base_path}<label class="field"><span>Path</span><input value={source.base_path} readonly /></label>{/if}
          </div>
          <div class="field-actions">
            <button type="submit" class="primary-button" disabled={busy}>Save changes</button>
            <button type="button" class="ghost-button" disabled={busy} onclick={() => editing = null}>Cancel</button>
          </div>
        </form>
      {/if}
    {/each}
  </div>

  <form class="field-form" onsubmit={addSource}>
    <h3>Add a music source</h3>
    <label class="field">
      <span>Type</span>
      <select bind:value={kind}>
        <option value="smb">SMB / network share</option>
        <option value="opensubsonic">OpenSubsonic server</option>
        <option value="podcast">Podcast (RSS feed)</option>
        <option value="archive">Internet Archive item</option>
      </select>
    </label>
    {#if kind === "smb"}
      <div class="field-grid">
        <label class="field"><span>Host</span><input name="host" placeholder="192.168.1.10" required /></label>
        <label class="field"><span>Share</span><input name="share" placeholder="Music" required /></label>
        <label class="field"><span>Path (optional)</span><input name="base_path" placeholder="Albums" /></label>
        <label class="field"><span>Name (optional)</span><input name="display_name" placeholder="NAS" /></label>
        <label class="field"><span>User (optional)</span><input name="username" /></label>
        <label class="field"><span>Password (optional)</span><input name="password" type="password" /></label>
      </div>
    {:else if kind === "opensubsonic"}
      <div class="field-grid">
        <label class="field"><span>Server URL</span><input name="host" placeholder="https://navidrome.example.com" required /></label>
        <label class="field"><span>User</span><input name="username" required /></label>
        <label class="field"><span>Password</span><input name="password" type="password" required /></label>
        <label class="field"><span>Name (optional)</span><input name="display_name" placeholder="My server" /></label>
      </div>
    {:else if kind === "podcast"}
      <div class="field-grid">
        <label class="field"><span>Feed URL</span><input name="host" placeholder="https://feeds.example.com/show.xml" required /></label>
        <label class="field"><span>Name (optional)</span><input name="display_name" placeholder="My podcast" /></label>
      </div>
    {:else}
      <div class="field-grid">
        <label class="field"><span>Item</span><input name="host" placeholder="gd1977-05-08 or an archive.org/details/… URL" required /></label>
        <label class="field"><span>Name (optional)</span><input name="display_name" placeholder="Concert" /></label>
      </div>
    {/if}
    <div class="field-actions">
      <button type="submit" class="primary-button" disabled={busy}>Add source</button>
      <span class="form-status" class:error>{status}</span>
      {#if updatingSourceId}<button type="button" class="ghost-button" onclick={() => void refreshUpdatedSource()}>{sourceRefreshFailed ? "Retry check" : "Check status"}</button>{/if}
    </div>
  </form>
</section>
