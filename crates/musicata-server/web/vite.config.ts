import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { VitePWA } from "vite-plugin-pwa";
import { resolve } from "node:path";

// Two entry points mirror the two pages: `/` (player) and `/admin`. Vite emits one HTML +
// hashed bundle per entry into dist/, which the server embeds via rust-embed. base "/" so
// built HTML references assets at /assets/* (served by the asset route).
export default defineConfig({
  plugins: [
    svelte(),
    VitePWA({
      registerType: "autoUpdate",
      injectRegister: "auto",
      // Multi-page app: don't fall every navigation back to index.html, so /admin still
      // loads admin.html. The server serves the precached assets + HTML directly.
      workbox: {
        globPatterns: ["**/*.{js,css,html,svg}"],
        navigateFallback: null,
      },
      includeAssets: ["icon.svg"],
      manifest: {
        name: "Musicata",
        short_name: "Musicata",
        description: "Your music library and multi-room player.",
        categories: ["music", "entertainment"],
        start_url: "/",
        scope: "/",
        display: "standalone",
        background_color: "#16181d",
        theme_color: "#16181d",
        icons: [
          { src: "/icon.svg", sizes: "any", type: "image/svg+xml", purpose: "any maskable" },
        ],
      },
    }),
  ],
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
