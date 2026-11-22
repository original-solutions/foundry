import { expect, test } from "@playwright/test";
import { sessionLabel } from "../src/labels";

/**
 * The session's line in the sidebar once it took its task and went idle: its model, the title its
 * task gave it, then the idle state. A session that has not taken the task yet has no title. After
 * the reload, only the Stop that ends the turn can make it idle again.
 */
const idle = new RegExp(`^opus e2e work ${sessionLabel.idle}$`);

test("start a task, use its workspace's shell, and see the same screen after a reload", async ({ page }) => {
  await page.goto(`${process.env.FOUNDRY_E2E_URL}/`);
  await expect(page.getByRole("region", { name: "e2e-machine" })).toBeVisible();

  await page.getByRole("button", { name: "New task" }).click();
  const dialog = page.getByRole("dialog", { name: "New task" });
  // The task names the workspace.
  await dialog.getByRole("textbox", { name: "What should Claude work on?" }).fill("e2e work");
  await dialog.getByRole("radiogroup", { name: "Project" }).getByRole("radio", { name: "demo" }).check();
  await dialog.getByRole("button", { name: "Start" }).click();

  const workspace = page.getByRole("group", { name: "e2e work" });
  await expect(workspace).toBeVisible();
  // The fake agent takes the task as its first prompt and ends its turn.
  const session = workspace.getByRole("link", { name: idle });
  await expect(session).toBeVisible({ timeout: 60_000 });
  await session.click();
  await expect(page).toHaveURL(/\/terminals\/[0-9a-f-]+$/);
  await expect(page.getByTestId(`pane-${page.url().split("/").at(-1)}`).locator(".xterm-rows")).toContainText('PROMPT="e2e work"');

  // A workspace's actions show while the pointer is over it.
  await workspace.hover();
  await workspace.getByRole("button", { name: "New shell" }).click();
  const shell = workspace.getByRole("link", { name: /^shell/ });
  await shell.click();
  // The page is already on the session's terminal, so a check of the URL's shape would pass before
  // the move to the shell. The sidebar marks the link of the terminal the URL names.
  await expect(shell).toHaveAttribute("aria-current", "true");
  const url = page.url();
  // The session's terminal stays mounted, hidden, beside the shell's: read the shell's pane.
  const pane = page.getByTestId(`pane-${url.split("/").at(-1)}`);

  const rows = pane.locator(".xterm-rows");
  await expect(rows).toBeVisible();
  // Keys typed before the shell's prompt shows could reach a terminal not yet attached.
  await expect(rows).toContainText("$");
  await pane.locator(".xterm").click();
  await page.keyboard.type("echo e2e-$((6*7))");
  await page.keyboard.press("Enter");
  await expect(rows).toContainText("e2e-42");

  await page.reload();

  await expect(page).toHaveURL(url);
  const again = page.getByRole("group", { name: "e2e work" });
  await expect(again).toBeVisible();
  await expect(again.getByRole("link", { name: idle })).toBeVisible();
  await expect(again.getByRole("link", { name: /^shell/ })).toBeVisible();
  await expect(rows).toContainText("e2e-42");
});
