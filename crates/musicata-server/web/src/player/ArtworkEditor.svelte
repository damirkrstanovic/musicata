<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, type AlbumArtworkReview } from "../lib/api";
  import { session } from "../lib/session.svelte";
  import { onDestroy } from "svelte";

  let {
    albumId,
    expanded,
    onupdated,
  }: { albumId: string; expanded: boolean; onupdated: (artworkUrl: string | null) => void } = $props();

  let review = $state<AlbumArtworkReview | null>(null);
  let loading = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let status = $state<string | null>(null);
  let disposed = false;

  onDestroy(() => {
    disposed = true;
  });

  function message(error: unknown): string {
    return error instanceof Error ? error.message : "Artwork couldn’t be updated. Try again.";
  }

  function apply(next: AlbumArtworkReview, notice: string) {
    review = next;
    status = notice;
    error = null;
    if (!disposed) onupdated(next.selected_artwork_url);
  }

  async function load() {
    loading = true;
    error = null;
    try {
      review = await api.albumArtworkReview(albumId);
    } catch (cause) {
      error = message(cause);
    } finally {
      loading = false;
    }
  }

  $effect(() => {
    if (expanded && !review && !loading && !error) void load();
  });

  async function select(artworkId: string) {
    busy = true;
    error = null;
    status = "Saving artwork…";
    try {
      apply((await api.selectAlbumArtwork(albumId, artworkId))!, "Artwork selected.");
    } catch (cause) {
      error = message(cause);
    } finally {
      busy = false;
    }
  }

  async function upload(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    input.value = "";
    error = null;
    status = null;
    if (!new Set(["image/jpeg", "image/png", "image/webp", "image/gif"]).has(file.type)) {
      error = "Choose a JPEG, PNG, WebP, or GIF image.";
      return;
    }
    if (file.size > 16 * 1024 * 1024) {
      error = "Artwork must be 16 MB or smaller.";
      return;
    }
    busy = true;
    status = "Uploading artwork…";
    try {
      apply((await api.uploadAlbumArtwork(albumId, file))!, "Artwork uploaded.");
    } catch (cause) {
      error = message(cause);
    } finally {
      busy = false;
    }
  }

  async function clear() {
    busy = true;
    error = null;
    status = "Returning to automatic artwork…";
    try {
      apply((await api.clearAlbumArtwork(albumId))!, "Returned to automatic artwork.");
    } catch (cause) {
      error = message(cause);
    } finally {
      busy = false;
    }
  }

  async function refresh() {
    busy = true;
    error = null;
    status = "Refreshing artwork…";
    try {
      apply((await api.refreshAlbumArtwork(albumId))!, "Artwork refreshed.");
    } catch (cause) {
      error = message(cause);
    } finally {
      busy = false;
    }
  }
</script>

{#if session.isAdmin && expanded}
  <div class="artwork-editor">
    {#if expanded}
      <section class="artwork-workbench" id={`artwork-editor-${albumId}`} aria-label="Change album artwork" aria-busy={busy}>
        <header>
          <div>
            <h3>Artwork</h3>
            <p>Choose an image for this album. Your choice is kept until you clear it.</p>
          </div>
          <button class="ghost-button mini" type="button" disabled={busy} onclick={refresh}>Refresh artwork</button>
        </header>

        {#if loading}
          <p class="admin-hint" aria-live="polite">Loading artwork choices…</p>
        {:else if review}
          <div class="artwork-candidates" aria-label="Artwork candidates">
            {#each review.candidates as candidate (candidate.id)}
              <button
                class:selected={candidate.selected}
                class="artwork-candidate"
                type="button"
                disabled={busy || candidate.selected}
                aria-pressed={candidate.selected}
                onclick={() => select(candidate.id)}
              >
                <img src={candidate.preview_url} alt={`Use ${candidate.file_name} as artwork`} />
                <span>{candidate.file_name}</span>
                <small>{candidate.source === "local_file" ? "Folder image" : candidate.source}</small>
              </button>
            {:else}
              <p class="admin-hint">No candidates yet. Upload an image or refresh automatic artwork.</p>
            {/each}
          </div>
          <div class="artwork-actions">
            <label class="ghost-button artwork-upload" class:disabled={busy}>
              <span>Upload image</span>
              <input type="file" accept="image/jpeg,image/png,image/webp,image/gif" disabled={busy} onchange={upload} />
            </label>
            <button class="ghost-button danger" type="button" disabled={busy || !review.manual} onclick={clear}>
              Clear artwork
            </button>
          </div>
          <p class="admin-hint">Clear artwork removes your manual choice and returns to automatic artwork. Refresh preserves artwork you selected manually.</p>
        {/if}

        {#if status}<p class="artwork-status" aria-live="polite">{status}</p>{/if}
        {#if error}<p class="artwork-error" role="alert">{error}</p>{/if}
      </section>
    {/if}
  </div>
{/if}

<style>
  .artwork-editor { margin: 12px 0 20px; }
  .artwork-workbench { margin-top: 8px; max-width: 42rem; padding: 14px; border: 1px solid var(--line); border-radius: 12px; background: var(--panel); }
  header { display: flex; gap: 12px; align-items: start; justify-content: space-between; }
  header :global(.ghost-button) { flex-shrink: 0; white-space: nowrap; }
  .artwork-candidates > .admin-hint { grid-column: 1 / -1; }
  h3 { margin: 0; font: 600 1rem var(--font-ui); }
  header p { margin: 3px 0 0; color: var(--muted); font-size: .875rem; }
  .artwork-candidates { display: grid; grid-template-columns: repeat(auto-fill, minmax(112px, 1fr)); gap: 10px; margin-top: 14px; }
  .artwork-candidate { display: grid; gap: 5px; min-width: 0; padding: 6px; color: var(--text); text-align: left; background: transparent; border: 1px solid var(--line); border-radius: 10px; cursor: pointer; }
  .artwork-candidate:hover:not(:disabled), .artwork-candidate:focus-visible, .artwork-candidate.selected { border-color: var(--accent); background: var(--panel-strong); }
  .artwork-candidate:focus-visible { outline: 2px solid var(--accent-soft); outline-offset: 2px; }
  .artwork-candidate img { width: 100%; aspect-ratio: 1; object-fit: cover; border-radius: 6px; background: var(--panel-strong); }
  .artwork-candidate span, .artwork-candidate small { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .artwork-candidate span { font-size: .8125rem; }
  .artwork-candidate small { color: var(--muted); }
  .artwork-actions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 14px; }
  .artwork-actions :global(.ghost-button) { min-height: 44px; }
  .artwork-upload { display: inline-flex; align-items: center; cursor: pointer; }
  .artwork-upload.disabled { opacity: .55; cursor: not-allowed; }
  .artwork-upload:focus-within { outline: 2px solid var(--accent-soft); outline-offset: 2px; }
  .artwork-upload input { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); }
  .artwork-status { margin: 12px 0 0; color: var(--ok); }
  .artwork-error { margin: 12px 0 0; color: var(--danger); }
  @media (max-width: 480px) { header { align-items: stretch; flex-direction: column; } header :global(.ghost-button) { align-self: flex-start; min-height: 44px; } .artwork-candidates { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
</style>
