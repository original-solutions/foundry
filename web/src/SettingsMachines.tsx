import type { FC } from "react";
import { Link } from "@tanstack/react-router";
import { Page, type PageProps } from "./SettingsPage";

/**
 * Settings → Machines. A stub for now: the priority order, adding a machine and the update
 * checklist still live on the machines board.
 */
export const SettingsMachines: FC<PageProps> = () => (
  <Page title="Machines">
    <p className="text-sm text-neutral-400">
      Machines are added, ordered, limited and updated from{" "}
      <Link to="/machines" className="text-sky-300 hover:underline">
        The machines board
      </Link>
      .
    </p>
  </Page>
);
