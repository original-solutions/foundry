import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";
import { useEffect, useRef, type ReactNode } from "react";
import { factoryOverview } from "./api";
import type { DesktopStatus } from "./AlertsBell";
import { factoryKey } from "./factory";
import type { FleetState, MachineView } from "./fleet";
import type { Notify } from "./notify";
import { SettingsAppearance } from "./SettingsAppearance";
import { SettingsFactory } from "./SettingsFactory";
import { SettingsKeyboard } from "./SettingsKeyboard";
import { SettingsMachine } from "./SettingsMachine";
import { SettingsMachines } from "./SettingsMachines";
import { SettingsNotifications } from "./SettingsNotifications";
import { SettingsProjects } from "./SettingsProjects";
import { categoryOf, navEntries, type Category, type NavEntry } from "./settingsNav";
import { useTheme } from "./theme";

/** What every settings page is given: the fleet, the machines as the board shows them, the machine serving the page, and the notification choices. */
export interface PageProps {
  state: FleetState;
  machines: MachineView[];
  here: string | undefined;
  notify: { notify: Notify; desktop: DesktopStatus; onChoose: (change: Partial<Notify>) => void };
}

/** A page of settings: its title over its rows. */
export function Page({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section aria-label={title}>
      <h2 className="text-lg font-semibold text-neutral-100">{title}</h2>
      <div className="mt-3">{children}</div>
    </section>
  );
}

/**
 * One setting: its label and help on the left, its control on the right. A row being edited is
 * `changed`, marked until saved or discarded; one whose value changed elsewhere since it was edited
 * is `stale`. `note` is what is said under the control, such as a refusal.
 */
export function Row({
  label,
  help,
  htmlFor,
  changed = false,
  stale = false,
  note,
  children,
}: {
  label: string;
  help?: ReactNode;
  htmlFor?: string;
  changed?: boolean;
  stale?: boolean;
  note?: ReactNode;
  children: ReactNode;
}) {
  const mark = stale ? "border-l-amber-400" : changed ? "border-l-sky-400" : "border-l-transparent";
  return (
    <div data-row="" data-changed={changed || undefined} data-stale={stale || undefined} className={`grid gap-x-8 gap-y-2 border-b border-l-2 border-b-neutral-800 py-3.5 pl-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.2fr)] ${mark}`}>
      <div className="min-w-0">
        {htmlFor ? (
          <label htmlFor={htmlFor} className="block text-sm font-medium text-neutral-200">
            {label}
          </label>
        ) : (
          <div className="text-sm font-medium text-neutral-200">{label}</div>
        )}
        {help && <p className="mt-0.5 text-xs leading-4 text-neutral-500">{help}</p>}
      </div>
      <div className="min-w-0 self-center">
        {children}
        {note}
      </div>
    </div>
  );
}

const chipTone = { offline: "border-red-400/40 text-red-300", "update ready": "border-sky-400/40 text-sky-300", coordinator: "border-emerald-400/40 text-emerald-300" };

function Entry({ entry, active, machine }: { entry: NavEntry; active: boolean; machine: string | undefined }) {
  return (
    <div className="shrink-0">
      <Link
        to="/settings/$category"
        params={{ category: entry.category }}
        aria-current={active && machine === undefined ? "page" : undefined}
        className={`block rounded-md px-2.5 py-1.5 text-sm whitespace-nowrap min-[720px]:whitespace-normal ${active && machine === undefined ? "bg-neutral-800 text-neutral-50" : "text-neutral-300 hover:bg-neutral-900"}`}
      >
        <span className="font-medium">{entry.label}</span>
        {entry.summary && (
          <span title={entry.summary} className="mt-0.5 hidden truncate text-xs font-normal text-neutral-500 min-[720px]:block">
            {entry.summary}
          </span>
        )}
      </Link>
      {entry.machines && entry.machines.length > 0 && (
        <ul className="mt-0.5 ml-3 hidden border-l border-neutral-800 min-[720px]:block">
          {entry.machines.map((m) => (
            <li key={m.id}>
              <Link
                to="/settings/machines/$machine"
                params={{ machine: m.id }}
                aria-current={machine === m.id ? "page" : undefined}
                className={`flex items-baseline justify-between gap-2 rounded-md px-2.5 py-1 text-xs ${machine === m.id ? "bg-neutral-800 text-neutral-50" : "text-neutral-400 hover:bg-neutral-900"}`}
              >
                <span className="min-w-0 truncate">{m.name}</span>
                {m.chip && <span className={`shrink-0 rounded-full border px-1.5 text-[10px] ${chipTone[m.chip]}`}>{m.chip}</span>}
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/**
 * The Settings page (`/settings`): categories on the left in two groups, Fleet and This browser,
 * each with a line of its current values, and the page of the chosen category on the right. Below
 * 720px the nav is one row of categories that scrolls sideways above the page, with no values and
 * no machine entries. An unknown category shows Factory.
 */
export function SettingsPage({ notify, machines, here, state }: PageProps) {
  const { category: named, machine } = useParams({ strict: false });
  const category: Category = machine !== undefined ? "machines" : categoryOf(named);
  const client = useQueryClient();
  const theme = useTheme();
  const { data: overview } = useQuery({ queryKey: factoryKey, queryFn: factoryOverview, retry: false });

  // The overview follows the events socket, as the Factory view's does: read again when the settings or a status change.
  const seen = useRef({ statuses: state.factory, seq: state.settings.seq });
  useEffect(() => {
    if (seen.current.statuses === state.factory && seen.current.seq === state.settings.seq) return;
    seen.current = { statuses: state.factory, seq: state.settings.seq };
    void client.invalidateQueries({ queryKey: factoryKey });
  }, [client, state.factory, state.settings.seq]);

  const groups = navEntries({
    factory: overview?.settings,
    projects: state.settings.projects,
    projectFactory: state.settings.project_factory ?? {},
    machines,
    notify: notify.notify,
    theme,
  });
  const props: PageProps = { state, machines, here, notify };

  return (
    <div className="flex h-full flex-col min-[720px]:flex-row">
      <nav aria-label="Settings" className="flex shrink-0 gap-1 overflow-x-auto border-b border-neutral-800 p-2 min-[720px]:w-64 min-[720px]:flex-col min-[720px]:gap-0 min-[720px]:overflow-x-visible min-[720px]:overflow-y-auto min-[720px]:border-r min-[720px]:border-b-0 min-[720px]:p-3">
        <Link to="/" className="shrink-0 rounded-md px-2.5 py-1.5 text-sm whitespace-nowrap text-sky-300 hover:underline">
          ← Board
        </Link>
        {groups.map((group) => (
          <div key={group.title} className="flex gap-1 min-[720px]:mt-3 min-[720px]:flex-col min-[720px]:gap-0">
            <p className="hidden px-2.5 pb-1 text-[11px] font-semibold tracking-wider text-neutral-500 uppercase min-[720px]:block">{group.title}</p>
            {group.entries.map((entry) => (
              <Entry key={entry.category} entry={entry} active={entry.category === category} machine={entry.category === "machines" ? machine : undefined} />
            ))}
          </div>
        ))}
      </nav>
      <div className="min-h-0 min-w-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl px-6 pt-5">
          <PageFor category={category} machine={machine} props={props} />
        </div>
      </div>
    </div>
  );
}

function PageFor({ category, machine, props }: { category: Category; machine: string | undefined; props: PageProps }) {
  switch (category) {
    case "factory":
      return <SettingsFactory {...props} />;
    case "projects":
      return <SettingsProjects {...props} />;
    case "machines":
      return machine !== undefined ? <SettingsMachine {...props} machineId={machine} /> : <SettingsMachines {...props} />;
    case "notifications":
      return <SettingsNotifications {...props} />;
    case "appearance":
      return <SettingsAppearance {...props} />;
    case "keyboard":
      return <SettingsKeyboard {...props} />;
  }
}
