import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// The horsie session server binds to 127.0.0.1:3789 by default (no CORS layer),
// so in dev we proxy the whole `/api` surface — REST + both SSE streams
// (`/api/sessions/:id/events`, `/api/events`) — to it from the same origin.
const HORSIE_SERVER = process.env.HORSIE_SERVER ?? "http://127.0.0.1:3789";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5299,
    proxy: {
      "/api": {
        target: HORSIE_SERVER,
        changeOrigin: true,
        // SSE needs an unbuffered, long-lived upstream connection.
        configure: (proxy) => {
          proxy.on("proxyReq", (proxyReq) => {
            proxyReq.setHeader("Accept-Encoding", "identity");
          });
        },
      },
    },
  },
});
