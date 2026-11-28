import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import { KeySheet, keyGroups, useHelpKey } from "./KeySheet";
import { SettingsKeyboard } from "./SettingsKeyboard";
import { pageProps } from "./test-utils";

function Harness() {
  const [open, setOpen] = useState(false);
  useHelpKey(() => setOpen(true));
  return (
    <>
      <input aria-label="Task" />
      <div className="xterm">
        <textarea aria-label="Terminal input" />
      </div>
      <KeySheet open={open} onOpenChange={setOpen} />
    </>
  );
}

describe("KeySheet", () => {
  it("opens on ? and lists the keys, grouped by where they work", () => {
    render(<Harness />);

    fireEvent.keyDown(document.body, { key: "?" });

    const sheet = screen.getByRole("dialog", { name: "Keyboard" });
    for (const what of ["Jump to a terminal, a view or an action, or give Claude a new task", "Go to the next terminal that needs you", "Find in the open terminal", "Move between workspaces and terminals", "Next file"]) {
      expect(sheet.textContent).toContain(what);
    }
    expect(screen.getByRole("heading", { name: "The workspaces list" })).toBeDefined();
  });

  it("lists the keys of the task box, the palette's actions, find, a commit, a comment, a reply and the rail", () => {
    render(<Harness />);
    fireEvent.keyDown(document.body, { key: "?" });

    const sheet = screen.getByRole("dialog", { name: "Keyboard" });
    for (const what of [
      "Start the task",
      "Leave the board's empty task line",
      "Back from what can be done to the list",
      "The older match",
      "The newer match",
      "Earlier or later commit; before the first, all changes",
      "Add the comment on a line",
      "Drop the comment being written",
      "Send the reply",
      "Close the reply box",
      "Move along the collapsed rail",
      "Close what is on top: a dialog, sheet, drawer or card",
    ]) {
      expect(sheet.textContent).toContain(what);
    }
  });

  it("leaves ? alone while typing, in a field or in a terminal", () => {
    render(<Harness />);

    fireEvent.keyDown(screen.getByLabelText("Task"), { key: "?" });
    fireEvent.keyDown(screen.getByLabelText("Terminal input"), { key: "?" });

    expect(screen.queryByRole("dialog")).toBeNull();
  });
});

describe("keyGroups", () => {
  it("lists ⌘, for Settings under Anywhere", () => {
    const anywhere = keyGroups.find((g) => g.where === "Anywhere")!;
    expect(anywhere.keys.find(([, what]) => what === "Settings")?.[0].slice(1)).toEqual([","]);
  });
});

describe("SettingsKeyboard", () => {
  it("renders every group of keyGroups, read-only", () => {
    render(<SettingsKeyboard {...pageProps()} />);
    for (const group of keyGroups) expect(screen.getByRole("heading", { name: group.where })).toBeDefined();
    expect(screen.getByText("Settings")).toBeDefined();
    expect(screen.queryByRole("button")).toBeNull();
  });
});

