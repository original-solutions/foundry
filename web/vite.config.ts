import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// The daemon refuses a request whose Host is not its own or whose Origin differs from its Host, so
// the proxy keeps the browser's headers (changeOrigin off).
const api = { "/api": { target: process.env.FOUNDRY_API ?? "http://127.0.0.1:7433", ws: true, changeOrigin: false } };

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: { proxy: api },
  preview: { proxy: api },
  test: {
    include: ["src/**/*.test.{ts,tsx}"],
    environment: "jsdom",
    setupFiles: ["./src/test-setup.ts"],
    // theme.test.tsx reads these two as text, to check the light palette against Tailwind's.
    css: { include: [/src\/index\.css/, /tailwindcss\/theme\.css/] },
  },
});
