import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { resolve } from "node:path";

// Two entry points mirror the two pages: `/` (player) and `/admin`. Vite emits one HTML +
// hashed bundle per entry into dist/, which the server embeds via rust-embed. base "/" so
// built HTML references assets at /assets/* (served by the asset route).
export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        player: resolve(import.meta.dirname, "index.html"),
        admin: resolve(import.meta.dirname, "admin.html"),
      },
    },
  },
});
