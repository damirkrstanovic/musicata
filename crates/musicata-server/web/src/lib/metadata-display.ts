// SPDX-License-Identifier: AGPL-3.0-or-later
// Presentation fallbacks for malformed tags. These never alter canonical metadata.

export type AlbumDisplayHint = {
  folder_name: string | null;
};

const timestampName = /^(?:\d{10,13}|\d{4}[-_.]\d{2}[-_.]\d{2}(?:[ T_-]\d{2}(?::\d{2}(?::\d{2})?)?)?)$/;
const unknownAlbumTimestamp = /^unknown album\s*\(\s*\d{1,2}[/-]\d{1,2}[/-]\d{2,4}(?:\s+[^)]*)?\)$/i;
const punctuationOnly = /^[\s'"“”‘’`´]+$/;

function useful(value: string | null | undefined, placeholder: string): string | null {
  const text = value?.trim();
  if (
    !text ||
    text.toLocaleLowerCase() === placeholder ||
    (placeholder === "unknown album" && unknownAlbumTimestamp.test(text)) ||
    punctuationOnly.test(text) ||
    timestampName.test(text)
  ) {
    return null;
  }
  return text;
}

export function albumTitleForDisplay(title: string, hint?: AlbumDisplayHint | null): string {
  return useful(title, "unknown album") ?? useful(hint?.folder_name, "unknown album") ?? "Unidentified album";
}

export function artistNameForDisplay(name: string): string {
  return useful(name, "unknown artist") ?? "Unknown artist";
}

export function folderIdentityForDisplay(title: string, hint?: AlbumDisplayHint | null): string | null {
  return useful(title, "unknown album") ? null : useful(hint?.folder_name, "unknown album");
}
