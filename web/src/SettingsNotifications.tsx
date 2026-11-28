import type { FC } from "react";
import { blockedHint, Choice, type DesktopStatus } from "./AlertsBell";
import { Page, type PageProps } from "./SettingsPage";

const permission: Record<DesktopStatus, string> = { unsupported: "not supported", blocked: "blocked", ask: "not asked yet", allowed: "allowed" };

/**
 * Settings → Notifications: how news of a session that finishes or needs you reaches you. Cards in
 * the page, desktop alerts (with what the browser's permission says beside them) and the catch-up
 * after a while away. Each saves the moment it changes, in this browser.
 */
export const SettingsNotifications: FC<PageProps> = ({ notify: { notify, desktop, onChoose } }) => {
  const desktopOn = notify.desktop && desktop === "allowed";
  return (
    <Page title="Notifications">
      <p className="text-xs leading-4 text-neutral-500">When a session you are not looking at finishes its turn or needs you. Each one reaches you once.</p>
      <div className="mt-3 flex flex-col gap-1 rounded-lg border border-neutral-800 p-1.5">
        <Choice label="Cards in the page" checked={notify.cards} onChange={(cards) => onChoose({ cards })} disabled={false}>
          While you are at Foundry: what Claude said or asks, to open or answer.
        </Choice>
        <Choice label="Desktop alerts" checked={desktopOn} onChange={(on) => onChoose({ desktop: on })} disabled={desktop === "blocked" || desktop === "unsupported"}>
          {blockedHint[desktop] ?? "While Foundry is in the background, instead of a card. Cleared when you come back."}
        </Choice>
        <p className="px-2.5 pb-1.5 text-xs text-neutral-400">Browser permission: {permission[desktop]}</p>
        <Choice label="Catch-up when you come back" checked={notify.away} onChange={(away) => onChoose({ away })} disabled={false}>
          After two minutes away: what each session did meanwhile.
        </Choice>
      </div>
      <p className="mt-3 text-[11px] leading-4 text-neutral-500">Always on: the tab's title and icon show what needs you, and a screen reader hears each one.</p>
    </Page>
  );
};
