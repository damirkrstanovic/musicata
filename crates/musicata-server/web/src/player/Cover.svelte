<script lang="ts">
  // A robust cover image: shows a monogram immediately, lazily loads the artwork via an
  // IntersectionObserver (more reliable than native `loading="lazy"` for dynamically-rendered
  // grids), fades it in on load, and on error/abort retries a few times before settling on the
  // monogram — so a cover is never a broken or permanently-blank <img>, even if its first
  // request is cancelled by navigation, slow (cold network source), or transiently 404s.
  import { sizedArtwork, initial } from "../lib/dom";

  let { url, size = 300, label = "" }: { url: string | null; size?: number; label?: string } =
    $props();

  const MAX_RETRIES = 3;
  let host = $state<HTMLElement>();
  let near = $state(false);
  let loaded = $state(false);
  let attempt = $state(0);
  let retryTimer: ReturnType<typeof setTimeout> | undefined;

  const base = $derived(sizedArtwork(url, size));
  // Only request once near the viewport, while we have a URL and retries remain. A retry appends
  // a cache-buster so the browser re-fetches (the cover is usually cached server-side by then)
  // rather than reusing the aborted/failed response.
  const src = $derived(
    near && base && attempt <= MAX_RETRIES
      ? attempt === 0
        ? base
        : `${base}${base.includes("?") ? "&" : "?"}retry=${attempt}`
      : null,
  );

  $effect(() => {
    if (!host) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          near = true;
          observer.disconnect();
        }
      },
      { rootMargin: "400px" },
    );
    observer.observe(host);
    return () => observer.disconnect();
  });

  $effect(() => () => clearTimeout(retryTimer));

  function onError() {
    loaded = false;
    if (attempt < MAX_RETRIES) {
      clearTimeout(retryTimer);
      // Back off a little between tries (transient cancels/cold fetches recover quickly).
      retryTimer = setTimeout(() => (attempt += 1), 600 * (attempt + 1));
    }
  }
</script>

<span class="cover" bind:this={host}>
  <span class="cover-mono" aria-hidden="true">{initial(label)}</span>
  {#if src}
    <img
      class="cover-img"
      class:loaded
      {src}
      alt=""
      decoding="async"
      onload={() => (loaded = true)}
      onerror={onError}
    />
  {/if}
</span>
