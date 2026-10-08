import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
import { defineConfig } from "vite";
import { contentPlugin } from "./scripts/content-plugin.ts";
import { highlightPlugin } from "./scripts/highlight.ts";

export default defineConfig({
  plugins: [highlightPlugin(), contentPlugin(), react()],
  resolve: { alias: { "@": resolve(import.meta.dirname, "src") } },
  build: { ssr: "src/entry-server.tsx", outDir: ".prerender", copyPublicDir: false },
  logLevel: "warn",
});
