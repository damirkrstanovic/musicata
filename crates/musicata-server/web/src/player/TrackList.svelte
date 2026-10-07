<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import type { TrackRow } from "../lib/api";
  import { formatTime } from "../lib/format";
  import { ApiError, api, type Playlist, type PlaylistDetail } from "../lib/api";
  import { enqueueTracks, playNext, playTracks, startAudioRadio } from "../lib/playback";
  import { player } from "../lib/player.svelte";
  import { nav } from "../lib/nav.svelte";
  import { favorites } from "../lib/favorites.svelte";
  import { metadata } from "../lib/metadata.svelte";

  // Clicking a row plays the whole list starting there (matches the old player).
  let { tracks, playlistId = null, onPlaylistChanged = (_detail: PlaylistDetail) => {} }: {
    tracks: TrackRow[];
    playlistId?: string | null;
    onPlaylistChanged?: (detail: PlaylistDetail) => void;
  } = $props();
  let playlists = $state<Playlist[]>([]);
  let playlistBusy = $state(false);
  let playlistError = $state<string | null>(null);
  let retryPlaylistEdit = $state<(() => Promise<void>) | null>(null);

  $effect(() => {
    let alive = true;
    api.playlists().then((items) => { if (alive) playlists = items; }).catch(() => {});
    return () => { alive = false; };
  });

  async function runPlaylistEdit(action: () => Promise<void>) {
    if (playlistBusy) return;
    playlistBusy = true;
    playlistError = null;
    retryPlaylistEdit = null;
    try {
      await action();
    } catch (error) {
      if (error instanceof ApiError && error.status === 409 && playlistId) {
        const refreshed = await api.playlistDetail(playlistId);
        onPlaylistChanged(refreshed);
        playlistError = "Playlist changed elsewhere and was refreshed. Try your reorder again.";
        return;
      }
      playlistError = error instanceof Error ? error.message : "Couldn’t update playlist.";
      retryPlaylistEdit = action;
    } finally {
      playlistBusy = false;
    }
  }

  function addToPlaylist(event: Event, track: TrackRow) {
    const select = event.currentTarget as HTMLSelectElement;
    const id = select.value;
    if (!id) return;
    void runPlaylistEdit(async () => {
      const detail = await api.addToPlaylist(id, [track.id]);
      if (detail && id === playlistId) onPlaylistChanged(detail);
      select.value = "";
    });
  }

  function movePlaylistTrack(index: number, direction: -1 | 1) {
    if (!playlistId) return;
    const next = [...tracks];
    const expected = tracks.map((track) => track.id);
    const to = index + direction;
    [next[index], next[to]] = [next[to], next[index]];
    void runPlaylistEdit(async () => {
      const detail = await api.updatePlaylist(playlistId, {
        track_ids: next.map((track) => track.id),
        expected_track_ids: expected,
      });
      if (detail) onPlaylistChanged(detail);
    });
  }

  function removePlaylistTrack(index: number) {
    if (!playlistId) return;
    const expected = tracks.map((track) => track.id);
    void runPlaylistEdit(async () => {
      const detail = await api.updatePlaylist(playlistId, { remove_indices: [index], expected_track_ids: expected });
      if (detail) onPlaylistChanged(detail);
    });
  }
</script>

<div class="track-list">
  {#each tracks as track, index (`${track.id}-${index}`)}
    <div class="track" class:active={player.nowPlaying?.track_id === track.id}>
      <button
        type="button"
        class="track-play"
        title="Play from here"
        aria-label="Play {track.title}"
        onclick={() => playTracks(tracks, index)}>▶</button
      >
      <div
        class="track-main"
        role="button"
        tabindex="0"
        onclick={() => playTracks(tracks, index)}
        onkeydown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            playTracks(tracks, index);
          }
        }}
      >
        <span class="track-titles">
          <strong>{track.title}</strong>
          <button
            type="button"
            class="track-link"
            disabled={!track.artist_id}
            onclick={(e) => {
              e.stopPropagation();
              nav.push({ name: "artist", id: track.artist_id, label: track.artist_name });
            }}>{track.artist_name}</button
          >
        </span>
        <button
          type="button"
          class="track-link track-album-cell"
          disabled={!track.album_id}
          onclick={(e) => {
            e.stopPropagation();
            nav.push({ name: "album", id: track.album_id, title: track.album_title });
          }}>{track.album_title}</button
        >
      </div>
      <span class="track-stat">{track.duration_seconds ? formatTime(track.duration_seconds) : ""}</span>
      <span class="track-actions">
        <button
          class="icon-toggle heart"
          class:active={favorites.has(track.id)}
          type="button"
          title="Favorite"
          aria-pressed={favorites.has(track.id)}
          onclick={() => favorites.toggleTrack(track.id)}>{favorites.has(track.id) ? "♥" : "♡"}</button
        >
        <button
          class="icon-button track-audio-radio"
          type="button"
          title="Start audio radio — tracks that sound like this"
          onclick={() => startAudioRadio(track.id)}>≈</button
        >
        <button class="icon-button track-play-next" type="button" title="Play next" onclick={() => playNext(track)}>↳</button>
        <button class="icon-button track-add-queue" type="button" title="Add to queue" onclick={() => enqueueTracks([track])}>＋</button>
        <select class="track-add-playlist" aria-label="Add {track.title} to playlist" value="" disabled={playlistBusy} onchange={(event) => addToPlaylist(event, track)}>
          <option value="" disabled>Add to playlist</option>
          {#each playlists as playlist (playlist.id)}<option value={playlist.id}>{playlist.name}</option>{/each}
        </select>
        {#if playlistId}
          <button class="icon-button playlist-move-up" type="button" title="Move up" disabled={playlistBusy || index === 0} onclick={() => movePlaylistTrack(index, -1)}>↑</button>
          <button class="icon-button playlist-move-down" type="button" title="Move down" disabled={playlistBusy || index === tracks.length - 1} onclick={() => movePlaylistTrack(index, 1)}>↓</button>
          <button class="icon-button playlist-remove" type="button" title="Remove from playlist" disabled={playlistBusy} onclick={() => removePlaylistTrack(index)}>×</button>
        {/if}
        <button
          class="icon-button track-meta"
          type="button"
          title="Edit metadata"
          onclick={() => metadata.open(track.id)}>⋯</button
        >
      </span>
    </div>
  {/each}
</div>
{#if playlistError}
  <p class="admin-hint" role="alert">{playlistError} <button class="ghost-button playlist-edit-retry" type="button" disabled={playlistBusy || !retryPlaylistEdit} onclick={() => retryPlaylistEdit && void runPlaylistEdit(retryPlaylistEdit)}>Retry</button></p>
{/if}
