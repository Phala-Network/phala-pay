import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  resolve: { alias: { "@": resolve(import.meta.dirname, "src") } },
  build: { ssr: "src/entry-server.tsx", outDir: ".prerender", copyPublicDir: false },
  logLevel: "warn",
});
