<script lang="ts">
  import Modal from "../lib/Modal.svelte";
  import SourcesPanel from "./SourcesPanel.svelte";
  import SettingsPanel from "./SettingsPanel.svelte";
  import PlayersZonesPanel from "./PlayersZonesPanel.svelte";
  import MergedArtistsPanel from "./MergedArtistsPanel.svelte";
  import IdentificationPanel from "./IdentificationPanel.svelte";
  import ActivityPanel from "./ActivityPanel.svelte";
</script>

<header class="admin-bar">
  <div class="admin-brand">
    <span class="brand-mark">M</span>
    <div>
      <h1>Musicata</h1>
      <p>Administration</p>
    </div>
  </div>
  <a class="ghost-button" href="/v2">← Back to player</a>
</header>

<main class="admin-grid">
  <SourcesPanel />
  <SettingsPanel />
  <PlayersZonesPanel />
  <MergedArtistsPanel />
  <IdentificationPanel />
  <ActivityPanel />
</main>

<Modal />
