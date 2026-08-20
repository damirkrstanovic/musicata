<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onDestroy } from "svelte";
  import { api, type IdentificationStats, type SourceView, type Activity } from "../lib/api";
  import type { LibrarySummary } from "../types/LibrarySummary";
  import { connectActivity } from "../lib/activity";
  import { pct } from "../lib/format";

  let summary = $state<LibrarySummary | null>(null);
  let stats = $state<IdentificationStats | null>(null);
  let sources = $state<SourceView[]>([]);
  let running = $state<Activity[]>([]);

  // Throughput: compare successive snapshots (work done since last poll).
  let prev: { processed: number; enriched: number; t: number } | null = null;
  let idRate = $state(0); // tracks identified / min
  let enrichRate = $state(0); // tracks enriched / min

  async function refresh() {
    try {
      const [sum, st, src] = await Promise.all([
        api.librarySummary(),
        api.identificationStats(),
        api.sources(),
      ]);
      summary = sum;
      sources = src;
      const now = Date.now();
      if (prev) {
        const dt = (now - prev.t) / 1000;
        if (dt > 0) {
          idRate = Math.max(0, Math.round(((st.processed - prev.processed) / dt) * 60));
          enrichRate = Math.max(0, Math.round(((st.enrichment.enriched - prev.enriched) / dt) * 60));
        }
      }
      prev = { processed: st.processed, enriched: st.enrichment.enriched, t: now };
      stats = st;
    } catch {
      // keep last
    }
  }
  refresh();
  const timer = setInterval(refresh, 3000);
  const disconnect = connectActivity((items) => (running = items.filter((a) => a.status === "running")));
  onDestroy(() => {
    clearInterval(timer);
    disconnect();
  });

  const CAPS: [keyof SourceView["capabilities"], string][] = [
    ["can_scan", "Scan"],
    ["can_browse", "Browse"],
    ["can_search", "Search"],
    ["can_stream", "Stream"],
  ];
</script>

<section class="admin-panel admin-panel-wide">
  <div class="admin-panel-head">
    <h2>Library status</h2>
    {#if running.length}<span class="activity-live">working…</span>{/if}
  </div>

  {#if summary && stats}
    <div class="ident-stats">
      <div class="ident-stat"><span class="ident-num">{summary.track_count}</span><span class="ident-label">Tracks</span></div>
      <div class="ident-stat"><span class="ident-num">{summary.album_count}</span><span class="ident-label">Albums</span></div>
      <div class="ident-stat"><span class="ident-num">{summary.artist_count}</span><span class="ident-label">Artists</span></div>
      <div class="ident-stat"><span class="ident-num">{sources.length}</span><span class="ident-label">Sources</span></div>
    </div>

    {#snippet bar(label: string, done: number, total: number, queued: number, rate: number)}
      <div class="status-row">
        <div class="status-row-head">
          <span>{label} — <strong>{done.toLocaleString()}</strong> / {total.toLocaleString()} ({pct(done, total)}%)</span>
          <span>
            {#if queued > 0}<span class="status-queued">{queued.toLocaleString()} queued</span>{/if}
            {#if rate > 0}<span class="status-rate">· {rate}/min</span>{/if}
          </span>
        </div>
        <div class="bar"><span style="width:{pct(done, total)}%"></span></div>
      </div>
    {/snippet}

    {@render bar("Identification", stats.tracks.identified, stats.tracks.total, stats.queued, idRate)}
    {@render bar("Metadata enrichment", stats.enrichment.enriched, stats.enrichment.identified, stats.enrichment.queued, enrichRate)}
    {@render bar("Album covers", stats.artwork.album_covers, stats.artwork.albums_total, 0, 0)}
    {@render bar("Artist images", stats.artwork.artist_images, stats.artwork.artists_total, 0, 0)}

    <h3>Sources</h3>
    <div class="admin-list">
      {#each sources as source (source.id)}
        <div class="admin-row">
          <div class="admin-row-main">
            <div class="admin-row-title">
              <span class="dot online"></span><strong>{source.display_name}</strong><span class="tag">{source.kind}</span>
            </div>
            <div class="chips">
              {#each CAPS as [key, cap] (key)}
                {#if source.capabilities[key]}<span class="chip">{cap}</span>{/if}
              {/each}
            </div>
          </div>
        </div>
      {/each}
    </div>

    <h3>Happening now</h3>
    {#if running.length}
      <div class="activity-list">
        {#each running as item (item.id)}
          <div class="activity-item status-running">
            <div class="activity-head">
              <span class="activity-status">⟳</span><span class="activity-label">{item.label}</span>
            </div>
          </div>
        {/each}
      </div>
    {:else}
      <p class="admin-hint">Idle — all background work is up to date.</p>
    {/if}
  {:else}
    <p class="admin-hint">Loading…</p>
  {/if}
</section>

<style>
  .status-row {
    margin: 0.7rem 0;
  }
  .status-row-head {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    font-size: 0.85rem;
    margin-bottom: 4px;
  }
  .bar {
    height: 8px;
    border-radius: 4px;
    background: rgba(255, 255, 255, 0.08);
    overflow: hidden;
  }
  .bar > span {
    display: block;
    height: 100%;
    background: var(--accent, #e0a44c);
    transition: width 0.4s ease;
  }
  .status-queued {
    opacity: 0.7;
  }
  .status-rate {
    color: var(--accent, #e0a44c);
  }
</style>
