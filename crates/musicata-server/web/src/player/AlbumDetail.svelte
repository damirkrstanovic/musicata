<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, type AlbumDetail } from "../lib/api";
  import { session } from "../lib/session.svelte";
  import Cover from "./Cover.svelte";
  import { playTracks } from "../lib/playback";
  import { albumTitleForDisplay, artistNameForDisplay, folderIdentityForDisplay } from "../lib/metadata-display";
  import ArtworkEditor from "./ArtworkEditor.svelte";
  import TrackList from "./TrackList.svelte";

  let { id }: { id: string } = $props();
  let detail = $state<AlbumDetail | null>(null);
  let artworkExpanded = $state(false);
  let failed = $state(false);
  let showOriginalMetadata = $state(false);

  // Refetch when navigating between albums.
  $effect(() => {
    const albumId = id;
    let alive = true;
    detail = null;
    artworkExpanded = false;
    failed = false;
    showOriginalMetadata = false;
    api
      .albumDetail(albumId)
      .then((d) => {
        if (alive) detail = d;
      })
      .catch(() => {
        if (alive) failed = true;
      });
    return () => {
      alive = false;
    };
  });
</script>

{#if detail}
  {@const displayTitle = albumTitleForDisplay(detail.album.title, detail.display_hint)}
  {@const displayArtist = artistNameForDisplay(detail.album.artist_name)}
  {@const folderIdentity = folderIdentityForDisplay(detail.album.title, detail.display_hint)}
  <section class="detail-hero">
    <div class="hero-cover">
      <Cover url={detail.album.artwork_url} size={400} label={displayTitle} />
    </div>
    <div class="hero-info">
      <h2 class="hero-title" title={detail.album.title}>{displayTitle}</h2>
      <p class="hero-sub">
        {displayArtist}{detail.album.year ? ` · ${detail.album.year}` : ""} · {detail.tracks.length}
        tracks
      </p>
      {#if folderIdentity}<p class="admin-hint">Folder: {folderIdentity}</p>{/if}
      {#if displayTitle !== detail.album.title || displayArtist !== detail.album.artist_name}
        <button
          class="ghost-button"
          type="button"
          aria-expanded={showOriginalMetadata}
          aria-controls="album-original-metadata"
          onclick={() => (showOriginalMetadata = !showOriginalMetadata)}>Original metadata</button
        >
        {#if showOriginalMetadata}
          <div id="album-original-metadata" class="admin-hint">
            <div>Album: {detail.album.title}</div>
            <div>Artist: {detail.album.artist_name}</div>
            <div>Each track’s actions menu includes its source observations.</div>
          </div>
        {/if}
      {/if}
      <div class="hero-actions">
        <button class="primary-button" type="button" onclick={() => detail && playTracks(detail.tracks, 0)}>
          Play
        </button>
        {#if session.isAdmin}
          <button class="ghost-button artwork-disclosure" type="button"
            aria-expanded={artworkExpanded} aria-controls={`artwork-editor-${detail.album.id}`}
            onclick={() => (artworkExpanded = !artworkExpanded)}>Change artwork</button>
        {/if}
      </div>
    </div>
  </section>
      {#key id}
        <ArtworkEditor
          albumId={detail.album.id}
          expanded={artworkExpanded}
          onupdated={(artworkUrl) => {
            if (detail) detail.album.artwork_url = artworkUrl;
          }}
        />
      {/key}
  <TrackList tracks={detail.tracks} />
{:else if failed}
  <p class="admin-hint">Couldn't load this album. Try again.</p>
{:else}
  <p class="admin-hint">Loading…</p>
{/if}

<style>
  .artwork-disclosure { min-height: 44px; }
</style>
