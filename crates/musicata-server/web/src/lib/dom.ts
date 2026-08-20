// SPDX-License-Identifier: AGPL-3.0-or-later
// A `use:onVisible` action: calls `callback` whenever the node scrolls near the viewport.
// Used as an infinite-scroll sentinel at the end of a list.
export function onVisible(node: HTMLElement, callback: () => void) {
  const observer = new IntersectionObserver(
    (entries) => {
      if (entries.some((e) => e.isIntersecting)) callback();
    },
    { rootMargin: "600px" },
  );
  observer.observe(node);
  return {
    destroy() {
      observer.disconnect();
    },
  };
}

/** Append `?size=` so the server returns a thumbnail rather than the full cover. */
export function sizedArtwork(url: string | null, size: number): string | null {
  if (!url) return null;
  return `${url}${url.includes("?") ? "&" : "?"}size=${size}`;
}

/** First letter for a placeholder cover. */
export function initial(text: string): string {
  return (text.trim()[0] ?? "♪").toUpperCase();
}
