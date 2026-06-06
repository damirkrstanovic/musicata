<script lang="ts">
  import { onDestroy } from "svelte";
  import { api, type Activity } from "../lib/api";
  import { connectActivity } from "../lib/activity";
  import { timeAgo } from "../lib/format";

  let items = $state<Activity[]>([]);

  const ICONS: Record<string, string> = { running: "⟳", ok: "✓", interrupted: "⚠" };
  const live = $derived(items.some((a) => a.status === "running"));

  // HTTP fetch for the initial paint (in case the socket is slow), then live updates.
  api
    .activity()
    .then((a) => {
      if (!items.length) items = a;
    })
    .catch(() => {});
  const disconnect = connectActivity((next) => (items = next));
  onDestroy(disconnect);
</script>

<section class="admin-panel admin-panel-wide">
  <div class="admin-panel-head">
    <h2>Activity</h2>
    {#if live}<span class="activity-live">live</span>{/if}
  </div>

  <div class="activity-list">
    {#each items as item (item.id)}
      <div class="activity-item status-{item.status}">
        <div class="activity-head">
          <span class="activity-status">{ICONS[item.status] ?? "✕"}</span>
          <span class="activity-label">{item.label}</span>
          <span class="activity-when">
            {item.status === "running" ? "running…" : timeAgo(item.finished_at_unix_seconds ?? item.started_at_unix_seconds)}
          </span>
        </div>
        {#if item.message}<pre class="activity-message">{item.message}</pre>{/if}
      </div>
    {/each}
  </div>
</section>
