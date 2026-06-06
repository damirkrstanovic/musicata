<script lang="ts">
  import { api, ApiError, type AppSettings } from "../lib/api";

  let settings = $state<AppSettings>({
    artwork_fetch: false,
    fingerprint_enabled: false,
    musicbrainz_enrich_enabled: false,
    fanart_tv_key: "",
  });
  let status = $state("");
  let error = $state(false);
  let busy = $state(false);

  async function load() {
    try {
      settings = await api.settings();
    } catch {
      // keep defaults
    }
  }
  load();

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
</section>
