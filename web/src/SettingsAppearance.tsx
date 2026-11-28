import type { FC } from "react";
import { Page, Row, type PageProps } from "./SettingsPage";
import { setTheme, useTheme, type Theme } from "./theme";
import { button } from "./Modal";

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
        <div role="radiogroup" aria-label="Theme" className="flex gap-2">
          {themes.map((t) => (
            <button key={t.theme} type="button" role="radio" aria-checked={theme === t.theme} onClick={() => setTheme(t.theme)} className={`${button} aria-checked:border-sky-400/50 aria-checked:bg-sky-400/10 aria-checked:text-sky-200`}>
              {t.label}
            </button>
          ))}
        </div>
      </Row>
    </Page>
  );
};
