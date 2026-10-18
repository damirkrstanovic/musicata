<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  let { name }: { name: "play" | "pause" | "next" | "previous" | "queue" | "shuffle" | "repeat" } = $props();
</script>
<svg viewBox="0 0 24 24" aria-hidden="true">
  {#if name === "play"}<path d="m8 4 12 8-12 8Z"/>
  {:else if name === "pause"}<path d="M7 4v16M17 4v16" stroke-width="4"/>
  {:else if name === "next"}<path d="m5 4 12 8-12 8ZM20 4v16"/>
  {:else if name === "previous"}<path d="m19 4-12 8 12 8ZM4 4v16"/>
  {:else if name === "queue"}<path d="M4 6h16M4 12h12M4 18h8"/>
  {:else if name === "shuffle"}<path d="M3 6h3c5 0 7 12 12 12h3M18 15l3 3-3 3M3 18h3c2 0 4-3 6-6s4-6 6-6h3M18 3l3 3-3 3"/>
  {:else}<path d="M4 11V8a3 3 0 0 1 3-3h13M17 2l3 3-3 3M20 13v3a3 3 0 0 1-3 3H4M7 16l-3 3 3 3"/>{/if}
</svg>
<style>
  svg { width:22px; height:22px; fill:none; stroke:currentColor; stroke-width:1.7; stroke-linecap:round; stroke-linejoin:round; vertical-align:middle; flex:none; }
</style>
