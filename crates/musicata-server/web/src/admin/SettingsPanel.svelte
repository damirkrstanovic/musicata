<script lang="ts">
  import { api, ApiError, type AppSettings } from "../lib/api";

  let settings = $state<AppSettings>({
    artwork_fetch: false,
    fingerprint_enabled: false,
    musicbrainz_enrich_enabled: false,
    fanart_tv_key: "",
    ml_enabled: false,
    ml_service_url: "",
    ml_schedule: "02:00",
    history_enabled: true,
    scrobble_enabled: false,
    listenbrainz_token: "",
  });
  let status = $state("");
  let error = $state(false);
  let busy = $state(false);

  // Two-step confirm for the destructive clear (no native confirm() — see project conventions).
  let confirmClear = $state(false);
  let historyStatus = $state("");

  async function clearHistory() {
    if (!confirmClear) {
      confirmClear = true;
      return;
    }
    confirmClear = false;
    historyStatus = "Clearing…";
    try {
      const res = await api.clearHistory();
      historyStatus = `Cleared ${res?.removed ?? 0} listens.`;
    } catch (e) {
      historyStatus = e instanceof ApiError ? e.message : String(e);
    }
  }

  async function load() {
    try {
      settings = await api.settings();
    } catch {
      // keep defaults
    }
  }
  load();

  // Audio-analysis (musicata-ml) status + manual run.
  let mlStatus = $state<{ enabled: boolean; analyzed: number; total: number } | null>(null);
  let mlMsg = $state("");

  async function loadMlStatus() {
    try {
      mlStatus = await api.mlStatus();
    } catch {
      mlStatus = null;
    }
  }
  loadMlStatus();

  async function runMl() {
    mlMsg = "Starting…";
    try {
      await api.mlAnalyze();
      mlMsg = "Analysis started — the count below should climb.";
      setTimeout(loadMlStatus, 1500);
    } catch (e) {
      mlMsg = e instanceof ApiError ? e.message : String(e);
    }
  }

  async function save(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    status = "Saving…";
    error = false;
    try {
      await api.saveSettings({ ...settings, fanart_tv_key: settings.fanart_tv_key.trim() });
      status = "Saved.";
    } catch (e) {
      status = e instanceof ApiError ? e.message : String(e);
      error = true;
    } finally {
      busy = false;
    }
  }
</script>

<section class="admin-panel">
  <div class="admin-panel-head"><h2>Artwork &amp; identification</h2></div>
  <p class="admin-hint">All on-device. Identification uses AcoustID + MusicBrainz; files are never modified.</p>

  <form class="field-form" onsubmit={save}>
    <label class="toggle-row">
      <input type="checkbox" bind:checked={settings.artwork_fetch} />
      <span>Automatically find missing album covers</span>
    </label>
    <label class="toggle-row">
      <input type="checkbox" bind:checked={settings.fingerprint_enabled} />
      <span>Identify untitled/untagged tracks by their sound</span>
    </label>
    <label class="toggle-row">
      <input type="checkbox" bind:checked={settings.musicbrainz_enrich_enabled} />
      <span>Fill in real titles, artists &amp; albums for identified tracks</span>
    </label>
    <label class="field">
      <span>fanart.tv API key (optional)</span>
      <input bind:value={settings.fanart_tv_key} placeholder="personal key" />
    </label>
    <div class="field-actions">
      <button type="submit" class="primary-button" disabled={busy}>Save</button>
      <span class="form-status" class:error>{status}</span>
    </div>
  </form>

  <div class="admin-panel-head"><h2>Audio analysis (“sounds-like”)</h2></div>
  <p class="admin-hint">
    Analyzes tracks with the optional <code>musicata-ml</code> service to power audio “sounds-like”
    radio. Runs daily at the time below, or on demand. CPU-heavy, one-time per track. The service
    URL is <code>http://ml:3091</code> in the Docker stack (or <code>http://localhost:3091</code>
    standalone).
  </p>
  <form class="field-form" onsubmit={save}>
    <label class="toggle-row">
      <input type="checkbox" bind:checked={settings.ml_enabled} />
      <span>Analyze tracks for “sounds-like” recommendations</span>
    </label>
    <label class="field">
      <span>Analysis service URL</span>
      <input bind:value={settings.ml_service_url} placeholder="http://ml:3091" />
    </label>
    <label class="field">
      <span>Daily run time</span>
      <input type="time" bind:value={settings.ml_schedule} />
    </label>
    <div class="field-actions">
      <button type="submit" class="primary-button" disabled={busy}>Save</button>
      <span class="form-status" class:error>{status}</span>
    </div>
  </form>
  <div class="field-form">
    <div class="field-actions">
      <button type="button" class="ghost-button" onclick={runMl}>Run analysis now</button>
      <button type="button" class="ghost-button" onclick={loadMlStatus}>Refresh</button>
      <span class="form-status">{mlMsg}</span>
    </div>
    {#if mlStatus}
      <p class="admin-hint">
        {mlStatus.analyzed.toLocaleString()} of {mlStatus.total.toLocaleString()} tracks analyzed{mlStatus.enabled
          ? ""
          : " — analysis is turned off above"}.
      </p>
    {/if}
  </div>

  <div class="admin-panel-head"><h2>Listening history</h2></div>
  <p class="admin-hint">
    History stays on this device and powers your stats, recently/most played, and recommendations.
  </p>
  <div class="field-form">
    <label class="toggle-row">
      <input type="checkbox" bind:checked={settings.history_enabled} />
      <span>Record what I play (turn off for private listening)</span>
    </label>
    <p class="admin-hint">Saved with the settings above. Turning it off keeps existing history until you clear it.</p>
    <div class="field-actions">
      <button type="button" class="ghost-button danger" class:confirming={confirmClear} onclick={clearHistory}>
        {confirmClear ? "Confirm — clear all history?" : "Clear listening history"}
      </button>
      <span class="form-status">{historyStatus}</span>
    </div>
  </div>

  <div class="admin-panel-head"><h2>Scrobbling</h2></div>
  <p class="admin-hint">
    Optionally submit your plays to <a href="https://listenbrainz.org" target="_blank" rel="noreferrer">ListenBrainz</a>.
    Paste your token from listenbrainz.org → Settings. Submitted in the background; needs history on.
  </p>
  <form class="field-form" onsubmit={save}>
    <label class="toggle-row">
      <input type="checkbox" bind:checked={settings.scrobble_enabled} />
      <span>Scrobble my listens to ListenBrainz</span>
    </label>
    <label class="field">
      <span>ListenBrainz token</span>
      <input bind:value={settings.listenbrainz_token} placeholder="user token" />
    </label>
    <div class="field-actions">
      <button type="submit" class="primary-button" disabled={busy}>Save</button>
      <span class="form-status" class:error>{status}</span>
    </div>
  </form>
</section>

<style>
  .ghost-button.danger {
    color: var(--danger);
    border-color: var(--danger);
  }
  .ghost-button.danger.confirming {
    background: var(--danger);
    color: #fff;
  }
</style>
