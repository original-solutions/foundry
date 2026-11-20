import { expect, test } from "@playwright/test";

test("create a workspace, use its shell, and see the same screen after a reload", async ({ page }) => {
  await page.goto(`${process.env.FOUNDRY_E2E_URL}/`);
  await expect(page.getByRole("region", { name: "e2e-machine" })).toBeVisible();

  await page.getByRole("button", { name: "New workspace" }).click();
  await page.getByLabel("Project").selectOption({ label: "demo" });
  await page.getByLabel("Name").fill("e2e work");
  await page.getByRole("button", { name: "Create" }).click();

  const workspace = page.getByRole("group", { name: "e2e work" });
  await expect(workspace).toBeVisible();
  const session = workspace.getByRole("link", { name: /opus\s*Idle/ });
  await expect(session).toBeVisible({ timeout: 60_000 });

  await workspace.getByRole("button", { name: "New shell" }).click();
  const shell = workspace.getByRole("link", { name: /^shell/ });
  await shell.click();
  await expect(page).toHaveURL(/\/terminals\/[0-9a-f-]+$/);

  const rows = page.locator(".xterm-rows");
  await expect(rows).toBeVisible();
  await page.locator(".xterm").click();
  await page.keyboard.type("echo e2e-$((6*7))");
  await page.keyboard.press("Enter");
  await expect(rows).toContainText("e2e-42");
  const url = page.url();

  await page.reload();

  await expect(page).toHaveURL(url);
  await expect(page.getByRole("group", { name: "e2e work" })).toBeVisible();
  await expect(page.getByRole("group", { name: "e2e work" }).getByRole("link", { name: /opus\s*Idle/ })).toBeVisible();
  await expect(page.getByRole("group", { name: "e2e work" }).getByRole("link", { name: /^shell/ })).toBeVisible();
  await expect(page.locator(".xterm-rows")).toContainText("e2e-42");
});
