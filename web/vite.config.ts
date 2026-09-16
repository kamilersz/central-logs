import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev workflow: run `npm run dev` in this directory. Vite serves the SPA on
// http://localhost:5173 and proxies /api to the Rust server on :18080 (or
// whatever CENTRAL_LOGS_HTTP_PORT is set to).
//
// Production: run `npm run build`, which writes to ../target/web-dist (see the
// `build.outDir` below). The Rust binary embeds that directory via rust-embed
// and serves the SPA at `/` while APIs stay at `/api/*`.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "../target/web-dist",
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://localhost:18080",
        changeOrigin: true,
      },
      "/v1": {
        target: "http://localhost:18080",
        changeOrigin: true,
      },
      "/health": {
        target: "http://localhost:18080",
        changeOrigin: true,
      },
      "/metrics": {
        target: "http://localhost:18080",
        changeOrigin: true,
      },
    },
  },
});
