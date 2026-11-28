import type { FactorySettings, Project, ProjectFactory } from "./api";
import { settingsLine } from "./factory";
import type { MachineView } from "./fleet";
import { plural } from "./labels";
import type { Notify } from "./notify";
import type { Theme } from "./theme";

/** The categories of the Settings page, as `/settings/$category` names them. */
export type Category = "factory" | "projects" | "machines" | "notifications" | "appearance" | "keyboard";

/** What the nav's summaries are built from: state the page already holds, nothing fetched for it. */
export interface NavInput {
  /** The fleet's factory settings, once read. */
  factory?: FactorySettings;
  projects: Project[];
  projectFactory: Record<string, ProjectFactory>;
  machines: MachineView[];
  notify: Notify;
  theme: Theme;
}

export interface NavEntry {
  category: Category;
  label: string;
  /** Its current values in a line, when it has any. */
  summary?: string;
  /** Under Machines: each machine, with the first of offline, update ready and coordinator that applies to it. */
  machines?: { id: string; name: string; chip?: "offline" | "update ready" | "coordinator" }[];
}

export interface NavGroup {
  title: "Fleet" | "This browser";
  entries: NavEntry[];
}

/** The categories in the order the nav lists them, with their labels. */
export const categories: { category: Category; label: string }[] = [
  { category: "factory", label: "Factory" },
  { category: "projects", label: "Projects" },
  { category: "machines", label: "Machines" },
  { category: "notifications", label: "Notifications" },
  { category: "appearance", label: "Appearance" },
  { category: "keyboard", label: "Keyboard" },
];

/** The category `name` names; the first, Factory, for anything that is none. */
export function categoryOf(name: string | undefined): Category {
  return categories.find((c) => c.category === name)?.category ?? "factory";
}

/** The first of offline, update ready and coordinator that applies to `machine`. */
function chip(machine: MachineView): NonNullable<NavEntry["machines"]>[number]["chip"] {
  if (machine.status === "offline") return "offline";
  if (machine.sample?.update_ready) return "update ready";
  if (machine.acting) return "coordinator";
  return undefined;
}

function notifySummary(notify: Notify): string {
  const on = [notify.cards && "cards", notify.desktop && "desktop", notify.away && "catch-up"].filter((s): s is string => s !== false);
  return on.length > 0 ? on.join(" · ") : "all off";
}

/** The nav's groups and entries, each with a line of its current values. */
export function navEntries(input: NavInput): NavGroup[] {
  const label = (category: Category) => categories.find((c) => c.category === category)!.label;
  const offline = input.machines.filter((m) => m.status === "offline").length;
  const factoryOn = input.projects.filter((p) => input.projectFactory[p.id]?.on).length;
  return [
    {
      title: "Fleet",
      entries: [
        { category: "factory", label: label("factory"), summary: input.factory && settingsLine(input.factory) },
        { category: "projects", label: label("projects"), summary: `${plural(input.projects.length, "project")} · factory on ${factoryOn}` },
        {
          category: "machines",
          label: label("machines"),
          summary: offline > 0 ? `${plural(input.machines.length, "machine")} · ${offline} offline` : plural(input.machines.length, "machine"),
          machines: input.machines.map((m) => {
            const c = chip(m);
            return { id: m.id, name: m.name, ...(c && { chip: c }) };
          }),
        },
      ],
    },
    {
      title: "This browser",
      entries: [
        { category: "notifications", label: label("notifications"), summary: notifySummary(input.notify) },
        { category: "appearance", label: label("appearance"), summary: input.theme },
        { category: "keyboard", label: label("keyboard") },
      ],
    },
  ];
}
