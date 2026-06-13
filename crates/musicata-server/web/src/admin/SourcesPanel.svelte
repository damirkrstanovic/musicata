<script lang="ts">
  import { api, ApiError, type SourceView } from "../lib/api";
  import { confirmAction } from "../lib/modal";

  let sources = $state<SourceView[]>([]);
  let status = $state("");
  let error = $state(false);
  let busy = $state(false);
  let kind = $state<"smb" | "opensubsonic">("smb");

  const CAPS: [keyof SourceView["capabilities"], string][] = [
    ["can_scan", "Scan"],
    ["can_browse", "Browse"],
    ["can_search", "Search"],
    ["can_stream", "Stream"],
  ];

  async function load() {
    try {
      sources = await api.sources();
    } catch (e) {
      sources = [];
    }
  }
  load();

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
      } else {
        await api.createSource({
          kind: "opensubsonic",
          host: text("host"),
          username: text("username") || undefined,
          password: text("password") || undefined,
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
        </div>
        {#if source.kind !== "local"}
          <button type="button" class="ghost-button danger" onclick={() => remove(source)}>Remove</button>
        {/if}
      </div>
    {/each}
  </div>

  <form class="field-form" onsubmit={addSource}>
    <h3>Add a music source</h3>
    <label class="field">
      <span>Type</span>
      <select bind:value={kind}>
        <option value="smb">SMB / network share</option>
        <option value="opensubsonic">OpenSubsonic server</option>
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
    {:else}
      <div class="field-grid">
        <label class="field"><span>Server URL</span><input name="host" placeholder="https://navidrome.example.com" required /></label>
        <label class="field"><span>User</span><input name="username" required /></label>
        <label class="field"><span>Password</span><input name="password" type="password" required /></label>
        <label class="field"><span>Name (optional)</span><input name="display_name" placeholder="My server" /></label>
      </div>
    {/if}
    <div class="field-actions">
      <button type="submit" class="primary-button" disabled={busy}>Add source</button>
      <span class="form-status" class:error>{status}</span>
    </div>
  </form>
</section>
