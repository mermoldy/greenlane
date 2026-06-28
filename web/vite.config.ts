import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Built assets land in web/dist, which the Rust binary embeds via rust-embed.
// `base: "./"` keeps asset URLs relative so they resolve when served from the
// embedded root. During `bun run dev`, /ws is proxied to the greenhub backend.
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: { outDir: "dist", emptyOutDir: true },
  server: {
    proxy: {
      "/ws": { target: "http://127.0.0.1:8080", ws: true, changeOrigin: true },
    },
  },
});
