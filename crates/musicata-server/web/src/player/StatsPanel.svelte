<script lang="ts">
  import { statsPanel } from "../lib/statsPanel.svelte";
  import { api, type HistoryStats } from "../lib/api";

  let stats = $state<HistoryStats | null>(null);
  let failed = $state(false);

  // Fetch fresh each time the panel opens (the figures are cheap to compute server-side).
  $effect(() => {
    if (!statsPanel.open) return;
    failed = false;
    api
      .historyStats()
      .then((s) => (stats = s))
      .catch(() => (failed = true));
  });

  const rows = $derived(
    stats
      ? [
          { label: "Plays", value: String(stats.total_plays) },
          { label: "Skips", value: String(stats.total_skips) },
          { label: "Tracks played", value: String(stats.distinct_tracks_played) },
          { label: "Plays · last 7 days", value: String(stats.plays_last_7_days) },
          { label: "Plays · last 30 days", value: String(stats.plays_last_30_days) },
          { label: "Current streak", value: `${stats.current_streak_days} d` },
          { label: "Longest streak", value: `${stats.longest_streak_days} d` },
          { label: "Sessions", value: String(stats.listening_sessions) },
          { label: "Longest session", value: `${stats.longest_session_plays} plays` },
          { label: "Favorite tracks", value: String(stats.favorite_tracks) },
          { label: "Favorite albums", value: String(stats.favorite_albums) },
          { label: "Favorite artists", value: String(stats.favorite_artists) },
        ]
      : [],
  );
</script>

{#if statsPanel.open}
  <section class="eq-drawer" aria-label="Listening stats">
    <header class="eq-head">
      <strong>Listening stats</strong>
      <button class="ghost-button" type="button" onclick={() => (statsPanel.open = false)}>
        Close
      </button>
    </header>
    {#if failed}
      <p class="eq-note">Couldn't load your stats right now.</p>
    {:else if !stats}
      <p class="eq-note">Loading…</p>
    {:else}
      <p class="eq-note">
        Your listening over the retained history window. A session is a run of plays under 30
        minutes apart; a streak is consecutive days with a play.
      </p>
      <div class="stats-grid">
        {#each rows as row (row.label)}
          <div class="stat-row">
            <span class="stat-label">{row.label}</span>
            <span class="stat-value">{row.value}</span>
          </div>
        {/each}
      </div>
    {/if}
  </section>
{/if}

<style>
  .stats-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 4px 18px;
    padding: 0 22px;
  }
  .stat-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 10px;
    padding: 7px 0;
    border-bottom: 1px solid var(--line);
  }
  .stat-label {
    color: var(--muted);
    font-size: 0.8rem;
  }
  .stat-value {
    font-variant-numeric: tabular-nums;
    font-weight: 600;
  }
</style>
