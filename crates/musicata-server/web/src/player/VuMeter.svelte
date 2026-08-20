<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { getAudio } from "../lib/playback";
  import { meter } from "../lib/meter.svelte";

  // Smoothed needle positions (0..1) with VU-style ballistics: fast rise, slow fall.
  let posL = $state(0);
  let posR = $state(0);
  let raf = 0;
  let prevT = 0;

  const DB_MIN = -42;
  const DB_MAX = 3;
  function posFromRms(x: number): number {
    const db = 20 * Math.log10(x + 1e-7);
    return Math.min(1, Math.max(0, (db - DB_MIN) / (DB_MAX - DB_MIN)));
  }
  function ballistic(cur: number, target: number, dt: number): number {
    const tau = target > cur ? 0.09 : 0.45; // needle inertia
    return cur + (target - cur) * (1 - Math.exp(-dt / tau));
  }
  function frame(t: number) {
    const dt = prevT ? Math.min(0.05, (t - prevT) / 1000) : 0.016;
    prevT = t;
    const lv = getAudio()?.levels();
    posL = ballistic(posL, lv ? posFromRms(lv.l) : 0, dt);
    posR = ballistic(posR, lv ? posFromRms(lv.r) : 0, dt);
    raf = requestAnimationFrame(frame);
  }
  // Only run the 60fps loop while the meter drawer is open; the work is wasted (and the
  // analyser untouched) when it's closed.
  $effect(() => {
    if (!meter.open) return;
    prevT = 0;
    raf = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(raf);
  });

  // Geometry: needle pivots at the bottom-centre and swings ±SWING° around vertical.
  const CX = 130;
  const CY = 128;
  const SWING = 52;
  const NEEDLE = 104;
  const TICK_R = 110;
  function pt(pos: number, r: number): { x: number; y: number } {
    const a = ((-90 + (pos - 0.5) * 2 * SWING) * Math.PI) / 180;
    return { x: CX + r * Math.cos(a), y: CY + r * Math.sin(a) };
  }
  function needlePath(pos: number): string {
    const t = pt(pos, NEEDLE);
    return `M ${CX} ${CY} L ${t.x.toFixed(1)} ${t.y.toFixed(1)}`;
  }
  function arcPath(p0: number, p1: number, r: number): string {
    const a = pt(p0, r);
    const b = pt(p1, r);
    return `M ${a.x.toFixed(1)} ${a.y.toFixed(1)} A ${r} ${r} 0 0 1 ${b.x.toFixed(1)} ${b.y.toFixed(1)}`;
  }

  const TICKS = [
    { pos: 0.0, label: "-20" },
    { pos: 0.22, label: "-10" },
    { pos: 0.4, label: "-7" },
    { pos: 0.56, label: "-5" },
    { pos: 0.7, label: "-3" },
    { pos: 0.82, label: "0" },
    { pos: 0.92, label: "+2" },
    { pos: 1.0, label: "+3" },
  ];
  const RED_START = 0.82;
</script>

{#snippet vu(pos: number, label: string, gid: string)}
  <figure class="vu-face">
    <svg viewBox="0 0 260 150" class="vu-svg" role="img" aria-label={`${label} channel level`}>
      <defs>
        <radialGradient id={gid} cx="50%" cy="118%" r="125%">
          <stop offset="0%" stop-color="#3479df" />
          <stop offset="55%" stop-color="#14488f" />
          <stop offset="100%" stop-color="#081d40" />
        </radialGradient>
        <filter id={`${gid}-glow`} x="-30%" y="-30%" width="160%" height="160%">
          <feDropShadow dx="0" dy="0" stdDeviation="1.1" flood-color="#eaf2ff" flood-opacity="0.8" />
        </filter>
      </defs>
      <rect x="2" y="2" width="256" height="146" rx="11" fill={`url(#${gid})`} stroke="#05112b" />
      <path d={arcPath(0, 1, TICK_R)} class="vu-arc" />
      <path d={arcPath(RED_START, 1, TICK_R)} class="vu-red" />
      {#each TICKS as t (t.label)}
        {@const a = pt(t.pos, TICK_R)}
        {@const b = pt(t.pos, TICK_R - 9)}
        {@const c = pt(t.pos, TICK_R - 21)}
        <line x1={a.x.toFixed(1)} y1={a.y.toFixed(1)} x2={b.x.toFixed(1)} y2={b.y.toFixed(1)} class="vu-tick" />
        <text x={c.x.toFixed(1)} y={c.y.toFixed(1)} class="vu-label">{t.label}</text>
      {/each}
      <text x={CX} y="70" class="vu-vu">VU</text>
      <text x="24" y="140" class="vu-ch">{label}</text>
      <path d={needlePath(pos)} class="vu-needle" filter={`url(#${gid}-glow)`} />
      <circle cx={CX} cy={CY} r="5" class="vu-pivot" />
    </svg>
  </figure>
{/snippet}

{#if meter.open}
  <section class="vu-drawer" aria-label="Output level meters">
    <header class="vu-head">
      <strong>Output level</strong>
      <button class="ghost-button" type="button" onclick={() => meter.toggle()}>Close</button>
    </header>
    <div class="vu-meters">
      {@render vu(posL, "L", "vu-grad-l")}
      {@render vu(posR, "R", "vu-grad-r")}
    </div>
    <p class="vu-note">Browser output, post-EQ. Needle ballistics emulate a VU meter.</p>
  </section>
{/if}
