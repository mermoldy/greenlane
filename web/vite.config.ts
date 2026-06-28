import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Built assets land in web/dist, which the Rust binary embeds via rust-embed.
// `base: "./"` keeps asset URLs relative so they resolve when served from the
// embedded root. During `bun run dev`, the backend's three routes are proxied to
// greenlane: /ws (WebSocket timeline), /info (System panel), and /detach. Run the
// backend with `--web-dir web/dist` so it disables the token (cross-origin dev).
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: { outDir: "dist", emptyOutDir: true },
  server: {
    proxy: {
      "/ws": { target: "http://127.0.0.1:8080", ws: true, changeOrigin: true },
      "/info": { target: "http://127.0.0.1:8080", changeOrigin: true },
      "/detach": { target: "http://127.0.0.1:8080", changeOrigin: true },
    },
  },
});
