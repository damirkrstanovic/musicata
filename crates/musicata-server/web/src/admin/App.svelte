<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import DiagnosticsPanel from "./DiagnosticsPanel.svelte";
  import StatusDashboard from "./StatusDashboard.svelte";
  import SourcesPanel from "./SourcesPanel.svelte";
  import ImportExportPanel from "./ImportExportPanel.svelte";
  import SettingsPanel from "./SettingsPanel.svelte";
  import PlayersZonesPanel from "./PlayersZonesPanel.svelte";
  import SnapcastPanel from "./SnapcastPanel.svelte";
  import MergedArtistsPanel from "./MergedArtistsPanel.svelte";
  import UsersPanel from "./UsersPanel.svelte";
  import AccountPanel from "./AccountPanel.svelte";
  import EqPanel from "../player/EqPanel.svelte";
  import { nav } from "../lib/nav.svelte";
  import { SETTINGS_CATEGORIES, SETTINGS_DESTINATIONS, type SettingsCategory } from "../lib/settings-nav";

  let {category = "sources"}: {category?: SettingsCategory} = $props();
  let search = $state("");
  const results = $derived(SETTINGS_DESTINATIONS.filter(item =>
    search.trim().toLowerCase().split(/\s+/).every(word => `${item.label} ${item.keywords} ${SETTINGS_CATEGORIES.find(c=>c.id===item.category)?.label}`.toLowerCase().includes(word))));

  function open(id: SettingsCategory) {
    search = "";
    nav.push({name: "settings", category: id});
  }
  function moveTab(event: KeyboardEvent, index: number) {
    let next: number;
    if (event.key === "ArrowRight") next = (index + 1) % SETTINGS_CATEGORIES.length;
    else if (event.key === "ArrowLeft") next = (index + SETTINGS_CATEGORIES.length - 1) % SETTINGS_CATEGORIES.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = SETTINGS_CATEGORIES.length - 1;
    else return;
    event.preventDefault();
    open(SETTINGS_CATEGORIES[next].id);
    document.getElementById(`settings-tab-${SETTINGS_CATEGORIES[next].id}`)?.focus();
  }
</script>

<div class="settings-workspace" data-settings>
  <label class="settings-search">Find a setting
    <input type="search" data-settings-search bind:value={search} placeholder="Search EQ, sources, scrobbling…" />
  </label>
  <div class="settings-tabs" role="tablist" aria-label="Settings categories">
    {#each SETTINGS_CATEGORIES as item, index}
      <button type="button" class="ghost-button" class:active={category === item.id}
        id="settings-tab-{item.id}" role="tab" aria-selected={category === item.id}
        aria-controls={search.trim() ? undefined : "settings-panel"} tabindex={category === item.id ? 0 : -1}
        data-settings-category={item.id} onclick={() => open(item.id)}
        onkeydown={event => moveTab(event, index)}>{item.label}</button>
    {/each}
  </div>
  {#if search.trim()}
    <section data-settings-results aria-label="Settings search results" aria-live="polite">
      {#each results as item}
        <button type="button" class="settings-result ghost-button" onclick={() => open(item.category)}>
          <strong>{item.label}</strong><span>{SETTINGS_CATEGORIES.find(c => c.id === item.category)?.label} →</span>
        </button>
      {:else}<p class="admin-hint">No settings found. Try “EQ”, “sources” or “history”.</p>{/each}
    </section>
  {:else}
    <div id="settings-panel" role="tabpanel" aria-labelledby="settings-tab-{category}" data-settings-panel={category}>
      {#if category === "sources"}<SourcesPanel /><ImportExportPanel />
      {:else if category === "outputs"}<PlayersZonesPanel /><SnapcastPanel />
      {:else if category === "sound"}<EqPanel management />
      {:else if category === "metadata"}<SettingsPanel group="metadata" /><MergedArtistsPanel />
      {:else if category === "history"}<SettingsPanel group="history" />
      {:else}<StatusDashboard /><DiagnosticsPanel /><UsersPanel /><AccountPanel /><SettingsPanel group="system" />{/if}
    </div>
  {/if}
</div>

<style>
  .settings-search { display: grid; gap: 0.5rem; color: var(--muted); }
  .settings-search input { width: 100%; padding: 0.8rem; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; color: var(--text); }
  .settings-tabs { display: flex; flex-wrap: wrap; gap: 0.5rem; margin: 1.2rem 0; }
  .settings-tabs .active { color: var(--accent); border-color: var(--accent); }
  [role="tabpanel"] { display: grid; gap: 1rem; }
  .settings-result { display: flex; justify-content: space-between; gap: 1rem; width: 100%; margin-bottom: 0.6rem; text-align: left; }
  .settings-result span { color: var(--muted); }
</style>
