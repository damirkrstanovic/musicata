<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import type { TrackRow } from "../lib/api";
  import { formatTime } from "../lib/format";
  import { playTracks, startAudioRadio } from "../lib/playback";
  import { player } from "../lib/player.svelte";
  import { nav } from "../lib/nav.svelte";
  import { favorites } from "../lib/favorites.svelte";
  import { metadata } from "../lib/metadata.svelte";

  // Clicking a row plays the whole list starting there (matches the old player).
  let { tracks }: { tracks: TrackRow[] } = $props();
</script>

<div class="track-list">
  {#each tracks as track, index (track.id)}
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
