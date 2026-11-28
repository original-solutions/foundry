import { Link } from "@tanstack/react-router";
import { Page, type PageProps } from "./SettingsPage";

/**
 * Settings → one machine's page, at `/settings/machines/$machine`. A stub for now: its name,
 * limits and update still live on the machines board.
 */
export function SettingsMachine({ machines, machineId }: PageProps & { machineId: string }) {
  const name = machines.find((m) => m.id === machineId)?.name ?? machineId;
  return (
    <Page title={name}>
      <p className="text-sm text-neutral-400">
        This machine's name, limits and update are changed from{" "}
        <Link to="/machines" className="text-sky-300 hover:underline">
          The machines board
        </Link>
        .
      </p>
    </Page>
  );
}
