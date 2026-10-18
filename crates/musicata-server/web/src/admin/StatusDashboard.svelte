<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { onDestroy } from "svelte";
  import { api, diagnosticStatus, type Activity, type IdentificationStats, type SourceView } from "../lib/api";
  import { connectActivity } from "../lib/activity";
  import { pct } from "../lib/format";
  import type { LibrarySummary } from "../types/LibrarySummary";
  import type { WorkerHealth } from "../types/WorkerHealth";

  type WorkState = "working" | "waiting" | "retrying" | "complete" | "unavailable" | "incomplete";
  let summary = $state<LibrarySummary | null>(null);
  let stats = $state<IdentificationStats | null>(null);
  let sources = $state<SourceView[]>([]);
  let workers = $state<WorkerHealth[]>([]);
  let activities = $state<Activity[]>([]);
  let unavailable = $state(false);
  let observedAt = $state(Math.floor(Date.now() / 1000));
  const QUEUE_OBSERVATION_MAX_AGE_SECONDS = 5 * 60;

  async function refresh() {
    try {
      const [nextSummary, nextStats, nextSources, diagnostics] = await Promise.all([
        api.librarySummary(), api.identificationStats(), api.sources(), diagnosticStatus(),
      ]);
      summary = nextSummary;
      stats = nextStats;
      sources = nextSources;
      workers = diagnostics.runtime.workers;
      observedAt = Math.floor(Date.now() / 1000);
      unavailable = false;
    } catch {
      unavailable = true;
    }
  }
  void refresh();
  const timer = setInterval(() => void refresh(), 3000);
  const disconnect = connectActivity((items) => (activities = items));
  onDestroy(() => { clearInterval(timer); disconnect(); });

  const CAPS: [keyof SourceView["capabilities"], string][] = [
    ["can_scan", "Scan"], ["can_browse", "Browse"], ["can_search", "Search"], ["can_stream", "Stream"],
  ];
  function pending(kinds: string[], queued: number) {
    return Math.max(queued, ...workers.filter(worker => kinds.includes(worker.kind)).map(worker => worker.queued ?? 0));
  }
  function hasStalledWorker(kinds: string[]) {
    return workers.some(worker => kinds.includes(worker.kind) && worker.stalled);
  }
  function queueDataUnavailable(kinds: string[]) {
    return workers.some(worker => kinds.includes(worker.kind) && worker.state !== "disabled" && worker.queued != null &&
      (worker.queue_observed_at == null || observedAt - worker.queue_observed_at > QUEUE_OBSERVATION_MAX_AGE_SECONDS));
  }
  function lastQueueObservation(kinds: string[]) {
    const observed = workers
      .filter(worker => kinds.includes(worker.kind) && worker.queue_observed_at != null)
      .map(worker => worker.queue_observed_at!);
    return observed.length ? new Date(Math.max(...observed) * 1000).toLocaleTimeString() : null;
  }
  function workState(kinds: string[], done: number, total: number, queued: number): WorkState {
    const relevant = workers.filter(worker => kinds.includes(worker.kind));
    if (hasStalledWorker(kinds)) return "unavailable";
    if (relevant.some(worker => worker.active > 0 || worker.state === "running")) return "working";
    if (relevant.some(worker => worker.state === "backing_off" || worker.next_retry_at != null)) return "retrying";
    if (relevant.length > 0 && relevant.every(worker => worker.state === "disabled")) return "unavailable";
    if ((pending(kinds, queued) > 0 || relevant.some(worker => worker.state.startsWith("waiting_"))) && queueDataUnavailable(kinds)) return "unavailable";
    if (pending(kinds, queued) > 0 || relevant.some(worker => worker.state.startsWith("waiting_"))) return "waiting";
    return done >= total ? "complete" : "incomplete";
  }
  function stateLabel(state: WorkState) { return state[0].toUpperCase() + state.slice(1); }
  function progress(done: number, total: number, queued: number) {
    if (!total) return 100;
    return queued > 0 ? Math.min(99, pct(done, total)) : pct(done, total);
  }

  const running = $derived(activities.filter(activity => activity.status === "running"));
  const enabledSources = $derived(sources.filter(source => source.enabled).length);
  const disabledSources = $derived(sources.length - enabledSources);
</script>

<section class="admin-panel admin-panel-wide" data-library-status>
  <div class="admin-panel-head">
    <h2>Library status</h2>
    {#if running.length}<span class="activity-live">Working</span>{/if}
  </div>

  {#if unavailable}
    <p class="status-unavailable">Library status is unavailable. Musicata will try again.</p>
    <button type="button" class="ghost-button" onclick={() => void refresh()}>Retry</button>
  {:else if summary && stats}
    <div class="ident-stats">
      <div class="ident-stat"><span class="ident-num">{summary.track_count}</span><span class="ident-label">Tracks</span></div>
      <div class="ident-stat"><span class="ident-num">{summary.album_count}</span><span class="ident-label">Albums</span></div>
      <div class="ident-stat"><span class="ident-num">{summary.artist_count}</span><span class="ident-label">Artists</span></div>
      <div class="ident-stat"><span class="ident-num">{enabledSources}</span><span class="ident-label">Enabled sources</span></div>
    </div>

    {#snippet progressRow(label: string, done: number, total: number, queued: number, kinds: string[])}
      {@const pendingCount = pending(kinds, queued)}
      {@const state = workState(kinds, done, total, queued)}
      {@const lastObserved = lastQueueObservation(kinds)}
      <div class="status-row" data-library-work={label.toLowerCase()}>
        <div class="status-row-head">
          <span><strong>{label}</strong> · {done.toLocaleString()} of {total.toLocaleString()}</span>
          <span class="work-state" data-state={state}>{stateLabel(state)}</span>
        </div>
        <div class="bar" aria-label={`${label}: ${progress(done, total, pendingCount)}%`}><span style="width:{progress(done, total, pendingCount)}%"></span></div>
        <p class="status-detail">
          {#if state === "unavailable" && hasStalledWorker(kinds)}Worker progress has stalled
          {:else if state === "unavailable" && queueDataUnavailable(kinds)}Queue data{lastObserved ? ` was last observed at ${lastObserved}` : " is unavailable"}
          {:else if state === "unavailable"}All relevant workers are disabled
          {:else if pendingCount > 0}{pendingCount.toLocaleString()} waiting
          {:else if state === "retrying"}Retrying after a temporary failure
          {:else if state === "waiting"}Waiting for scheduled work
          {:else if state === "incomplete"}Coverage incomplete; no queued work is reported
          {:else if !total}No items to process
          {:else}No items waiting{/if}
        </p>
      </div>
    {/snippet}

    {@render progressRow("Identification", stats.tracks.identified, stats.tracks.total, stats.queued, ["fingerprint", "musicbrainz"])}
    {@render progressRow("Metadata enrichment", stats.enrichment.enriched, stats.enrichment.identified, stats.enrichment.queued, ["musicbrainz"])}
    {@render progressRow("Album covers", stats.artwork.album_covers, stats.artwork.albums_total, 0, ["artwork"])}
    {@render progressRow("Artist images", stats.artwork.artist_images, stats.artwork.artists_total, 0, ["artwork"])}

    <h3>Sources</h3>
    <p class="admin-hint">{enabledSources} enabled{#if disabledSources}, {disabledSources} disabled{/if}. Connection health is checked when a source is added; its current connection state is not reported by the server.</p>
    <div class="admin-list">
      {#each sources as source (source.id)}
        <div class="admin-row" data-source-state={source.enabled ? "enabled" : "disabled"}>
          <div class="admin-row-main">
            <div class="admin-row-title">
              <span class="dot" class:online={source.enabled} class:offline={!source.enabled}></span>
              <strong>{source.display_name}</strong><span class="tag">{source.kind}</span>
              <span class="work-state">{source.enabled ? "Enabled" : "Disabled"}</span>
            </div>
            <div class="chips">
              {#each CAPS as [key, cap] (key)}
                {#if source.capabilities[key]}<span class="chip">{cap}</span>{/if}
              {/each}
            </div>
          </div>
        </div>
      {:else}<p class="admin-hint">No sources are configured.</p>{/each}
    </div>

    <h3>Current activity</h3>
    {#if running.length}
      <div class="activity-list">
        {#each running as item (item.id)}
          <div class="activity-item status-running">
            <div class="activity-head"><span class="activity-label">{item.label}</span><span class="work-state" data-state="working">Working</span></div>
          </div>
        {/each}
      </div>
    {:else}<p class="admin-hint">No library work is running.</p>{/if}
  {:else}<p class="admin-hint">Loading library status…</p>{/if}
</section>

<style>
  .status-row { margin: 1rem 0; }
  .status-row-head { display: flex; justify-content: space-between; gap: 1rem; font-size: 0.9rem; margin-bottom: 0.35rem; }
  .bar { height: 8px; border-radius: 4px; background: var(--panel-strong); overflow: hidden; }
  .bar > span { display: block; height: 100%; background: var(--accent); transition: width 0.4s ease; }
  .status-detail { color: var(--muted); font-size: 0.85rem; margin: 0.35rem 0 0; }
  .work-state { color: var(--muted); font-size: 0.8rem; white-space: nowrap; }
  .work-state[data-state="working"] { color: var(--accent); }
  .work-state[data-state="retrying"], .work-state[data-state="unavailable"] { color: var(--danger); }
  .status-unavailable { color: var(--danger); font-family: var(--font-mono); }
</style>
