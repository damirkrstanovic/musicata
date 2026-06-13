<script lang="ts">
  import { dsp } from "../lib/dsp.svelte";
  import { audioDevices } from "../lib/audioDevices.svelte";
  import { api } from "../lib/api";
  import { parseParametricEq } from "../lib/dsp";
  import { responseCurveDb, logFreqs } from "../lib/eqcurve";
  import autoeqPresets from "../lib/autoeq-presets.json";

  function addOutput() {
    audioDevices.addPreset(audioDevices.presets.length === 0 ? "Speakers" : "Headphones");
    audioDevices.refreshDevices();
  }

  // AutoEq headphone picker: search the bundled curated set of real ParametricEQ presets,
  // parse the chosen one (reusing the import parser), and save it as a headphones profile.
  const HP_MODELS = Object.keys(autoeqPresets as Record<string, string>).sort();
  let hpSearch = $state("");
  const hpMatches = $derived(
    hpSearch.trim()
      ? HP_MODELS.filter((m) => m.toLowerCase().includes(hpSearch.trim().toLowerCase())).slice(0, 8)
      : [],
  );
  async function pickHeadphone(model: string) {
    const text = (autoeqPresets as Record<string, string>)[model];
    const prof = parseParametricEq(text, model);
    if (prof.bands.length === 0) return;
    prof.kind = "headphones";
    await dsp.saveProfile(prof);
    hpSearch = "";
  }

  // Room correction: a speakers profile (no EQ bands needed) that carries a measured WAV
  // impulse response, applied via a ConvolverNode in the browser graph.
  async function newRoomProfile() {
    await dsp.saveProfile({
      id: `room-${Date.now().toString(36)}`,
      name: "Room correction",
      preampDb: 0,
      bands: [],
      kind: "speakers",
    });
  }
  async function uploadRoomIr(event: Event) {
    const file = (event.currentTarget as HTMLInputElement).files?.[0];
    const id = dsp.activeId;
    if (!file || !id) return;
    await api.uploadRoomIr(id, await file.arrayBuffer());
    await dsp.load(); // profile.roomIr now set → the App $effect re-applies → convolver loads
  }
  async function clearRoomIr() {
    const id = dsp.activeId;
    if (!id) return;
    await api.deleteRoomIr(id);
    await dsp.load();
  }

  let importName = $state("");
  let importText = $state("");
  let importError = $state("");

  function bandLabel(type: string): string {
    return type === "peaking" ? "PK" : type === "lowshelf" ? "LS" : "HS";
  }

  // EQ frequency-response curve. Log-x 20 Hz..20 kHz; y auto-scaled to a symmetric dB range.
  const FREQS = logFreqs(96);
  const CW = 280;
  const CH = 116;
  const PADX = 6;
  const curve = $derived(dsp.active ? responseCurveDb(dsp.active, FREQS) : null);
  const range = $derived(
    curve ? Math.min(24, Math.max(6, Math.ceil(Math.max(...curve.map((d) => Math.abs(d)))))) : 12,
  );
  function xAt(i: number): number {
    return PADX + (i / (FREQS.length - 1)) * (CW - 2 * PADX);
  }
  function xForFreq(f: number): number {
    return PADX + (Math.log(f / 20) / Math.log(1000)) * (CW - 2 * PADX);
  }
  function yForDb(db: number, r: number): number {
    return CH / 2 - (db / r) * (CH / 2 - 8);
  }
  const curvePath = $derived(
    curve
      ? curve
          .map((db, i) => `${i ? "L" : "M"} ${xAt(i).toFixed(1)} ${yForDb(db, range).toFixed(1)}`)
          .join(" ")
      : "",
  );
  const GRID = [100, 1000, 10000];
  const GRID_LABEL: Record<number, string> = { 100: "100", 1000: "1k", 10000: "10k" };

  async function doImport() {
    importError = "";
    const name = importName.trim() || "Imported preset";
    const prof = await dsp.importText(importText, name);
    if (!prof) {
      importError =
        "No filters found. Paste a ParametricEQ.txt — lines like 'Filter 1: ON PK Fc 100 Hz Gain 3 dB Q 1'.";
      return;
    }
    importName = "";
    importText = "";
  }
</script>

{#if dsp.panelOpen}
  <section class="eq-drawer" aria-label="Equalizer">
    <header class="eq-head">
      <strong>Equalizer</strong>
      <button class="ghost-button" type="button" onclick={() => (dsp.panelOpen = false)}>Close</button>
    </header>

    <label class="eq-toggle">
      <input
        type="checkbox"
        checked={dsp.leveling}
        onchange={(e) => dsp.setLeveling((e.currentTarget as HTMLInputElement).checked)}
      />
      <span>Volume leveling</span>
    </label>
    <p class="eq-note">
      Normalizes loudness across tracks (EBU R128) so quiet and loud songs play at the same
      level — great for shuffle and radio.
    </p>

    <div class="eq-divider"></div>

    <label class="eq-toggle">
      <input
        type="checkbox"
        checked={dsp.enabled}
        onchange={(e) => dsp.setEnabled((e.currentTarget as HTMLInputElement).checked)}
      />
      <span>Enable equalizer</span>
    </label>
    <p class="eq-note">Applies to this browser's audio output only.</p>

    <label class="eq-field">
      <span>Preset</span>
      <select
        value={dsp.activeId ?? ""}
        onchange={(e) => dsp.setActive((e.currentTarget as HTMLSelectElement).value || null)}
      >
        <option value="">None</option>
        {#each dsp.profiles as p (p.id)}
          <option value={p.id}>{p.name}</option>
        {/each}
      </select>
    </label>

    {#if dsp.active && curve}
      <div class="eq-curve-wrap">
        <svg viewBox={`0 0 ${CW} ${CH}`} class="eq-curve" role="img" aria-label="EQ response curve">
          <line x1={PADX} y1={CH / 2} x2={CW - PADX} y2={CH / 2} class="eq-grid-0" />
          {#each GRID as f (f)}
            <line x1={xForFreq(f).toFixed(1)} y1="4" x2={xForFreq(f).toFixed(1)} y2={CH - 4} class="eq-grid" />
            <text x={xForFreq(f).toFixed(1)} y={CH - 4} class="eq-grid-label">{GRID_LABEL[f]}</text>
          {/each}
          <text x={CW - PADX} y="11" class="eq-grid-label eq-grid-db">+{range}</text>
          <text x={CW - PADX} y={CH - 8} class="eq-grid-label eq-grid-db">-{range}</text>
          <path d={curvePath} class="eq-curve-line" />
        </svg>
      </div>
      <div class="eq-bands">
        <div class="eq-band-head">
          Preamp {dsp.active.preampDb.toFixed(1)} dB · {dsp.active.bands.length} band{dsp.active.bands
            .length === 1
            ? ""
            : "s"}
        </div>
        {#each dsp.active.bands as b, i (i)}
          <div class="eq-band">
            <span class="eq-b-type">{bandLabel(b.type)}</span>
            <span>{b.freq} Hz</span>
            <span class="eq-b-gain">{b.gain > 0 ? "+" : ""}{b.gain} dB</span>
            <span class="eq-b-q">Q {b.q}</span>
          </div>
        {/each}
        {#if dsp.isCustom(dsp.activeId)}
          <button class="ghost-button" type="button" onclick={() => dsp.remove(dsp.activeId!)}>
            Delete preset
          </button>
        {/if}
      </div>
    {/if}

    <details class="eq-import">
      <summary>Outputs (speakers / headphones)</summary>
      <p class="eq-note">
        Bind a device + EQ profile + volume to a one-tap output; switch between them from the
        footer. Each output remembers its own volume.
      </p>
      {#each audioDevices.presets as preset (preset.id)}
        <div class="eq-output">
          <input
            class="eq-name"
            value={preset.label}
            oninput={(e) =>
              audioDevices.updatePreset(preset.id, { label: (e.currentTarget as HTMLInputElement).value })}
          />
          <select
            value={preset.sinkId ?? ""}
            onchange={(e) =>
              audioDevices.updatePreset(preset.id, {
                sinkId: (e.currentTarget as HTMLSelectElement).value || null,
              })}
          >
            <option value="">System default device</option>
            {#each audioDevices.devices as d (d.deviceId)}
              <option value={d.deviceId}>{d.label || "Output device"}</option>
            {/each}
          </select>
          <select
            value={preset.profileId ?? ""}
            onchange={(e) =>
              audioDevices.updatePreset(preset.id, {
                profileId: (e.currentTarget as HTMLSelectElement).value || null,
              })}
          >
            <option value="">No EQ</option>
            {#each dsp.profiles as p (p.id)}
              <option value={p.id}>{p.name}</option>
            {/each}
          </select>
          <button class="ghost-button" type="button" onclick={() => audioDevices.removePreset(preset.id)}>
            Remove
          </button>
        </div>
      {/each}
      <button class="ghost-button" type="button" onclick={addOutput}>+ Add output</button>
    </details>

    <details class="eq-import">
      <summary>Room correction (speakers)</summary>
      <p class="eq-note">
        Apply a measured room-correction filter (a WAV impulse response from REW/DRC). Stereo;
        corrects bass/low-mids. Create a profile, then upload its IR.
      </p>
      {#if dsp.activeId && dsp.isCustom(dsp.activeId)}
        {#if dsp.active?.roomIr}
          <p class="eq-note">
            Impulse response loaded ({dsp.active.roomIr.sampleRate || "?"} Hz).
          </p>
          <button class="ghost-button" type="button" onclick={clearRoomIr}>Remove impulse response</button>
        {:else}
          <label class="eq-field">
            <span>Upload impulse response (.wav)</span>
            <input type="file" accept=".wav,audio/wav,audio/x-wav" onchange={uploadRoomIr} />
          </label>
        {/if}
      {:else}
        <button class="ghost-button" type="button" onclick={newRoomProfile}>
          + New room-correction profile
        </button>
      {/if}
    </details>

    <details class="eq-import">
      <summary>Pick your headphone (AutoEq)</summary>
      <p class="eq-note">
        Zero-effort correction — pick your model and it's applied instantly (real AutoEq presets;
        a preference target, so nudge the bass to taste). Not listed? Use the paste box below.
      </p>
      <input
        class="eq-name"
        type="text"
        placeholder="Search headphones (e.g. HD 600)"
        bind:value={hpSearch}
      />
      {#if hpMatches.length > 0}
        <ul class="hp-matches">
          {#each hpMatches as model (model)}
            <li>
              <button type="button" onclick={() => pickHeadphone(model)}>{model}</button>
            </li>
          {/each}
        </ul>
      {:else if hpSearch.trim()}
        <p class="eq-note">No match in the bundled set — paste its ParametricEQ below.</p>
      {/if}
    </details>

    <details class="eq-import">
      <summary>Import a headphone preset</summary>
      <p class="eq-note">
        Find your headphone on <strong>autoeq.app</strong> (or the AutoEq project) and paste its
        ParametricEQ text below.
      </p>
      <input class="eq-name" type="text" placeholder="Preset name (e.g. HD 600)" bind:value={importName} />
      <textarea
        class="eq-text"
        rows="6"
        spellcheck="false"
        placeholder={"Preamp: -6.8 dB\nFilter 1: ON PK Fc 21 Hz Gain 4.7 dB Q 0.7\n..."}
        bind:value={importText}
      ></textarea>
      {#if importError}<p class="eq-error">{importError}</p>{/if}
      <button class="primary-button" type="button" onclick={doImport}>Import &amp; apply</button>
    </details>
  </section>
{/if}

<style>
  .hp-matches {
    list-style: none;
    margin: 0.4rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .hp-matches button {
    width: 100%;
    text-align: left;
    background: transparent;
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 0.4rem;
    color: inherit;
    padding: 0.4rem 0.55rem;
    font-size: 0.82rem;
    cursor: pointer;
  }
  .hp-matches button:hover {
    background: rgba(212, 175, 55, 0.14);
  }
  .eq-output {
    display: grid;
    grid-template-columns: 1fr;
    gap: 0.3rem;
    padding: 0.5rem 0;
    border-bottom: 1px solid rgba(255, 255, 255, 0.07);
  }
  .eq-output select,
  .eq-output input {
    width: 100%;
  }
</style>
