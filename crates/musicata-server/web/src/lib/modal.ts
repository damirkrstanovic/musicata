// A promise-based modal, mirroring the old admin.js `openModal`. A single <Modal> instance
// (mounted in App) renders whatever is in `activeModal`; callers `await openModal(...)`.
import { writable } from "svelte/store";

export interface ModalField {
  key: string;
  label: string;
  type?: string;
  value?: string;
  placeholder?: string;
}

export interface ModalOptions {
  title?: string;
  message?: string;
  fields?: ModalField[];
  confirmLabel?: string;
  danger?: boolean;
}

export interface ActiveModal extends ModalOptions {
  resolve: (result: Record<string, string> | null) => void;
}

export const activeModal = writable<ActiveModal | null>(null);

/** Resolves to the field values (trimmed), or null if cancelled/dismissed. */
export function openModal(opts: ModalOptions): Promise<Record<string, string> | null> {
  return new Promise((resolve) => activeModal.set({ ...opts, resolve }));
}

/** A destructive confirmation. Resolves true when confirmed. */
export function confirmAction(opts: {
  title?: string;
  message?: string;
  confirmLabel?: string;
}): Promise<boolean> {
  return openModal({ confirmLabel: "Delete", danger: true, ...opts }).then((r) => r !== null);
}

/** A single text input. Resolves the trimmed value, or null if cancelled. */
export function promptText(opts: {
  title?: string;
  label: string;
  value?: string;
  placeholder?: string;
  confirmLabel?: string;
}): Promise<string | null> {
  return openModal({
    title: opts.title,
    confirmLabel: opts.confirmLabel ?? "Save",
    fields: [
      { key: "value", label: opts.label, value: opts.value, placeholder: opts.placeholder },
    ],
  }).then((r) => (r ? r.value : null));
}
