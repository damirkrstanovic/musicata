<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  // Reads only `player.elapsed`/`player.duration` — the hot-path signals. A progress tick
  // mutates those alone, so only this row's text + thumb move; nothing else in the footer
  // re-renders. While dragging, the thumb tracks the pointer (not the server echo).
  import { player } from "../lib/player.svelte";
  import { formatTime } from "../lib/format";
  import { sendCommand } from "../lib/commands";

  let dragValue = $state(0);
  const value = $derived(player.seekDragging ? dragValue : player.elapsed);
</script>

<div class="seek-row">
  <span class="time">{formatTime(value)}</span>
  <input
    class="seek"
    type="range"
    min="0"
    max={player.duration || 0}
    step="1"
    {value}
    aria-label="Seek"
    oninput={(e) => {
      player.seekDragging = true;
      dragValue = Number((e.currentTarget as HTMLInputElement).value);
    }}
    onchange={(e) => {
      const seconds = Number((e.currentTarget as HTMLInputElement).value);
      player.seekDragging = false;
      player.elapsed = seconds;
      sendCommand(player.target, { command: "seek", position_seconds: seconds });
    }}
  />
  <span class="time">{formatTime(player.duration)}</span>
</div>
