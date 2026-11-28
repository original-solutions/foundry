import type { FC } from "react";
import { Page, Row, type PageProps } from "./SettingsPage";
import { setTheme, useTheme, type Theme } from "./theme";
import { viewLook } from "./Modal";

const themes: { theme: Theme; label: string }[] = [
  { theme: "dark", label: "Dark" },
  { theme: "light", label: "Light" },
];

/**
 * Settings → Appearance: dark or light. It reads and sets the same state as the sidebar's theme
 * toggle, so each shows the other's change at once. Kept in this browser, the moment it changes.
 */
export const SettingsAppearance: FC<PageProps> = () => {
  const theme = useTheme();
  return (
    <Page title="Appearance">
      <Row label="Theme" help="Kept in this browser. The toggle beside the workspaces list sets the same choice.">
        {/* Built as the board's View switch is, so every radiogroup in the app looks alike. */}
        <div role="radiogroup" aria-label="Theme" className="inline-flex gap-0.5 rounded-lg border border-neutral-800 bg-neutral-900 p-0.5 text-xs">
          {themes.map((t) => (
            <button
              key={t.theme}
              type="button"
              role="radio"
              aria-checked={theme === t.theme}
              onClick={() => setTheme(t.theme)}
              className={`rounded-md px-2.5 py-1 font-medium transition-colors ${theme === t.theme ? viewLook.on : viewLook.off}`}
            >
              {t.label}
            </button>
          ))}
        </div>
      </Row>
    </Page>
  );
};
