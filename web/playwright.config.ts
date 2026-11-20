import { defineConfig } from "@playwright/test";

// Not part of `npm run check`: it runs a real daemon, so it needs the cargo build and Chrome.
// The global setup starts the daemon and the UI and puts their addresses in FOUNDRY_E2E_URL.
export default defineConfig({
  testDir: "e2e",
  workers: 1,
  fullyParallel: false,
  retries: 0,
  timeout: 180_000,
  expect: { timeout: 15_000 },
  reporter: "list",
  globalSetup: "./e2e/global-setup.ts",
  use: { channel: "chrome", trace: "retain-on-failure" },
});
