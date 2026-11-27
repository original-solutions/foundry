import { defineConfig } from "@playwright/test";

// Not part of `npm run check`: it runs a real daemon, so it needs the cargo build and Chrome. CI
// runs it (`npm --prefix web run e2e` in .github/workflows/check.yml); the factory gate does not.
// The global setup starts the daemon and the UI and puts their addresses in FOUNDRY_E2E_URL.
export default defineConfig({
  testDir: "e2e",
  workers: 1,
  fullyParallel: false,
  retries: 0,
  timeout: 180_000,
  expect: { timeout: 30_000 },
  reporter: "list",
  globalSetup: "./e2e/global-setup.ts",
  // A terminal pane draws with WebGL where it can, onto a canvas; with WebGL off it draws with the
  // DOM, whose rows the specs read the screen's text from.
  use: { channel: "chrome", trace: "retain-on-failure", launchOptions: { args: ["--disable-webgl"] } },
});
