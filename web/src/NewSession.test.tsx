import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { NewSession } from "./NewSession";
import { json, mockFetch, renderApp } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

describe("NewSession", () => {
  it("starts a session in the workspace with the model given, then closes", async () => {
    const calls = mockFetch(json(201, { id: "s2" }));
    await renderApp(<NewSession workspace="w1" name="fix login" />);

    fireEvent.click(await screen.findByRole("button", { name: "New session" }));
    const dialog = await screen.findByRole("dialog");
    expect((within(dialog).getByLabelText("Model") as HTMLInputElement).value).toBe("opus");
    fireEvent.change(within(dialog).getByLabelText("Model"), { target: { value: "sonnet" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls).toEqual([{ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "sonnet" } }]);
  });

  it("starts the session on the first prompt written, Ctrl+Enter in it starting", async () => {
    const calls = mockFetch(json(201, { id: "s2" }));
    await renderApp(<NewSession workspace="w1" name="fix login" />);

    fireEvent.click(await screen.findByRole("button", { name: "New session" }));
    const dialog = await screen.findByRole("dialog");
    const prompt = within(dialog).getByLabelText("First prompt");
    fireEvent.change(prompt, { target: { value: "Find why login redirects twice" } });
    fireEvent.keyDown(prompt, { key: "Enter", ctrlKey: true });

    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls).toEqual([{ url: "/api/workspaces/w1/sessions", method: "POST", body: { model: "opus", prompt: "Find why login redirects twice" } }]);
  });

  it("keeps the dialog open with the daemon's words when the session is refused", async () => {
    mockFetch(json(500, { error: "set FOUNDRY_GATEWAY_URL" }));
    await renderApp(<NewSession workspace="w1" name="fix login" />);

    fireEvent.click(await screen.findByRole("button", { name: "New session" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Start" }));

    expect((await within(dialog).findByRole("alert")).textContent).toBe("Could not start a session in fix login: set FOUNDRY_GATEWAY_URL.");
    expect(screen.getByRole("dialog")).toBeDefined();
  });

  it("is a fresh dialog when reopened after a refusal", async () => {
    mockFetch(json(500, { error: "set FOUNDRY_GATEWAY_URL" }));
    await renderApp(<NewSession workspace="w1" name="fix login" />);

    fireEvent.click(await screen.findByRole("button", { name: "New session" }));
    fireEvent.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Start" }));
    await screen.findByRole("alert");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    fireEvent.click(screen.getByRole("button", { name: "New session" }));
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("stays open when reopened before a cancelled start lands", async () => {
    let answer: (response: Response) => void = () => {};
    vi.stubGlobal(
      "fetch",
      vi.fn(() => new Promise<Response>((resolve) => (answer = resolve))),
    );
    await renderApp(<NewSession workspace="w1" name="fix login" />);

    fireEvent.click(await screen.findByRole("button", { name: "New session" }));
    fireEvent.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Start" }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    fireEvent.click(screen.getByRole("button", { name: "New session" }));
    answer(json(201, { id: "s2" }));
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(screen.getByRole("dialog")).toBeDefined();
  });
});

