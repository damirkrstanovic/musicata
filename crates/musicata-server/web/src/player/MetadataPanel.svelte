<script lang="ts">
  import { metadata } from "../lib/metadata.svelte";
  import type { MetadataFieldValue } from "../lib/api";

  function fmt(v: MetadataFieldValue): string {
    return v.kind === "text_list" ? v.value.join(", ") : String(v.value);
  }
</script>

{#if metadata.trackId}
  <section class="metadata-panel metadata-drawer" aria-label="Track metadata">
    <header class="queue-head">
      <strong>Metadata</strong>
      <div class="queue-head-actions">
        <button class="ghost-button" type="button" onclick={() => metadata.close()}>Close</button>
      </div>
    </header>

    {#if metadata.loading}
      <p class="admin-hint">Loading…</p>
    {:else if metadata.review}
      {@const c = metadata.review.canonical}
      <div class="metadata-body">
        <h3 class="hero-title">{c.title}</h3>
        <p class="admin-hint">
          {c.artist_name} · {c.album_title}{c.year ? ` · ${c.year}` : ""}
        </p>

        {#each metadata.review.observations as obs (obs.source + obs.observed_at_unix_seconds)}
          <div class="meta-obs">
            <h4>{obs.source}</h4>
            {#each obs.fields as field (field.field_name + field.source)}
              <div class="meta-field admin-row">
                <div class="admin-row-main">
                  <div class="admin-row-title"><strong>{field.field_name}</strong></div>
                  <span class="ident-label">{fmt(field.value)}</span>
                </div>
                <button
                  class="icon-button"
                  class:active={field.approval_state === "approved"}
                  type="button"
                  title="Approve"
                  onclick={() => metadata.setField(field, "approved")}>✓</button
                >
                <button
                  class="icon-button"
                  class:active={field.approval_state === "rejected"}
                  type="button"
                  title="Reject"
                  onclick={() => metadata.setField(field, "rejected")}>✕</button
                >
              </div>
            {/each}
          </div>
        {:else}
          <p class="admin-hint">No additional metadata sources for this track.</p>
        {/each}
      </div>
    {:else}
      <p class="admin-hint">No metadata available.</p>
    {/if}
  </section>
{/if}
