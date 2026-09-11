import { defineConfig } from "astro/config";

// Kaeru's web client: a static build (ADR-023). Astro templates the shell and
// bundles the framework-free client island; the output in `dist/` is embedded
// into the agent-web binary by rust-embed. Build-time only: the shipped binary
// never needs Node (C13/C18).
export default defineConfig({
  output: "static",
  // `astro dev` proxies the API to a locally running `agent-web` so the
  // client island can be developed against the real server.
  vite: {
    server: {
      proxy: {
        "/api": "http://127.0.0.1:8080",
      },
    },
  },
});
