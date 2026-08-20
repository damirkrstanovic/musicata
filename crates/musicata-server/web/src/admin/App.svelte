<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import Modal from "../lib/Modal.svelte";
  import StatusDashboard from "./StatusDashboard.svelte";
  import SourcesPanel from "./SourcesPanel.svelte";
  import ImportExportPanel from "./ImportExportPanel.svelte";
  import SettingsPanel from "./SettingsPanel.svelte";
  import PlayersZonesPanel from "./PlayersZonesPanel.svelte";
  import SnapcastPanel from "./SnapcastPanel.svelte";
  import MergedArtistsPanel from "./MergedArtistsPanel.svelte";
  import UsersPanel from "./UsersPanel.svelte";
  import AccountPanel from "./AccountPanel.svelte";
</script>

<header class="admin-bar">
  <div class="admin-brand">
    <span class="brand-mark">M</span>
    <div>
      <h1>Musicata</h1>
      <p>Administration</p>
    </div>
  </div>
  <a class="ghost-button" href="/">← Back to player</a>
</header>

<main class="admin-grid">
  <StatusDashboard />

  <h2 class="admin-group-title">Library &amp; sources</h2>
  <SourcesPanel />
  <ImportExportPanel />

  <h2 class="admin-group-title">Identification &amp; artwork</h2>
  <SettingsPanel />

  <h2 class="admin-group-title">Playback</h2>
  <PlayersZonesPanel />
  <SnapcastPanel />

  <h2 class="admin-group-title">Curation</h2>
  <MergedArtistsPanel />

  <h2 class="admin-group-title">Accounts</h2>
  <UsersPanel />
  <AccountPanel />

  <Modal />
</main>

<style>
  .admin-group-title {
    grid-column: 1 / -1;
    margin: 1.4rem 0 0;
    padding-bottom: 0.4rem;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
    font-size: 0.8rem;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    opacity: 0.65;
  }
</style>
