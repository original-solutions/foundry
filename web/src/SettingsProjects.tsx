import type { FC } from "react";
import { Link } from "@tanstack/react-router";
import { Page, type PageProps } from "./SettingsPage";

/**
 * Settings → Projects. A stub for now: each project's controls still live in the Projects dialog,
 * opened from the board's task box, and on the Factory view.
 */
export const SettingsProjects: FC<PageProps> = () => (
  <Page title="Projects">
    <p className="text-sm text-neutral-400">
      Projects are added, removed and switched on for the factory from the Projects dialog, opened from the task box on{" "}
      <Link to="/" className="text-sky-300 hover:underline">
        The board
      </Link>
      .
    </p>
  </Page>
);
