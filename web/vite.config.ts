import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The engine serves the built app. In development, Vite serves it and forwards everything else to the engine.
const ENGINE = process.env.TORRENTIAL_ENGINE ?? "http://127.0.0.1:47919";

// The engine refuses requests that name another origin, and through the proxy that is Vite's.
const toEngine = {
  target: ENGINE,
  changeOrigin: true,
  configure: (proxy: { on(event: "proxyReq", listener: (request: { removeHeader(name: string): void }) => void): void }) =>
    proxy.on("proxyReq", (request) => request.removeHeader("origin")),
};

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: { outDir: "dist", emptyOutDir: true, target: "es2022" },
  server: {
    host: "127.0.0.1",
    proxy: {
      "/api": toEngine,
      // Raw files of a download: /{40-hex torrent ID}/{file index}/{name}
      "^/[0-9a-f]{40}/": toEngine,
    },
  },
});
