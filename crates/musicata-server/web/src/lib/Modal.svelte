<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { tick } from "svelte";
  import { activeModal } from "./modal";

  let formEl = $state<HTMLFormElement | null>(null);
  let restoreFocus: HTMLElement | null = null;

  // Focus the first field (or the confirm button) when a modal opens; restore on close.
  $effect(() => {
    if ($activeModal) {
      restoreFocus = document.activeElement as HTMLElement | null;
      tick().then(() => formEl?.querySelector<HTMLElement>($activeModal?.danger ? ".modal-actions .ghost-button" : "input, .primary-button")?.focus());
    }
  });

  function close(result: Record<string, string> | null) {
    const current = $activeModal;
    activeModal.set(null);
    current?.resolve(result);
    void tick().then(() => restoreFocus?.isConnected && restoreFocus.focus());
  }

  function submit(event: Event) {
    event.preventDefault();
    const current = $activeModal;
    if (!current) return;
    const result: Record<string, string> = {};
    for (const field of current.fields ?? []) {
      const el = formEl?.elements.namedItem(field.key) as HTMLInputElement | null;
      result[field.key] = (el?.value ?? "").trim();
    }
    close(result);
  }
</script>

<svelte:window
  onkeydown={(event) => {
    if (!$activeModal) return;
    if (event.key === "Escape") { event.preventDefault(); close(null); }
    if (event.key === "Tab" && formEl) {
      const controls = [...formEl.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex="0"]')];
      const first = controls[0], last = controls.at(-1);
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
    }
  }}
/>

{#if $activeModal}
  <div
    class="modal-scrim"
    role="presentation"
    onclick={(event) => {
      if (event.target === event.currentTarget) close(null);
    }}
  >
    <div class="modal" role="dialog" aria-modal="true" aria-labelledby={$activeModal.title ? "modal-title" : undefined}>
      <form bind:this={formEl} onsubmit={submit}>
        {#if $activeModal.title}<h2 class="modal-title" id="modal-title">{$activeModal.title}</h2>{/if}
        {#if $activeModal.message}<p class="modal-message">{$activeModal.message}</p>{/if}
        {#if $activeModal.fields?.length}
          <div class="modal-fields">
            {#each $activeModal.fields as field (field.key)}
              <label class="field">
                <span>{field.label}</span>
                <input
                  name={field.key}
                  type={field.type ?? "text"}
                  value={field.value ?? ""}
                  placeholder={field.placeholder ?? ""}
                />
              </label>
            {/each}
          </div>
        {/if}
        <div class="modal-actions">
          <button type="button" class="ghost-button" onclick={() => close(null)}>Cancel</button>
          <button type="submit" class="primary-button" class:danger={$activeModal.danger}>
            {$activeModal.confirmLabel ?? "OK"}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}
