import { useNavigate } from "@tanstack/react-router";
import { BoardIcon, MachinesIcon, TimelineIcon } from "./Board";
import { railKey } from "./collapsed";
import { SparkIcon } from "./FirstPrompt";
import { setHome, useHome } from "./home";
import type { Command } from "./jump";
import type { Needing } from "./needing";
import { nextKey } from "./NextUp";
import { FactoryIcon } from "./FactoryMark";
import { categories } from "./settingsNav";
import { settingsKey } from "./shortcut";
import { PanelIcon } from "./Sidebar";
import { StateGlyph } from "./StateGlyph";
import { GearIcon } from "./ThemeButton";

interface Options {
  /** What is on screen now, so its view says "Here": a terminal, the home (the board or the timeline), the machines, the factory or Settings. */
  screen: "terminal" | "home" | "machines" | "factory" | "settings";
  /** The terminal that needs you next, and how many others do, when any does. */
  next?: { next: Needing; others: number };
  onNext: (terminal: string) => void;
  onCreate: () => void;
  onKeys: () => void;
  /** Collapses or expands the sidebar, where the screen is wide enough to keep it beside; `rail` says which it is now. */
  sidebar?: { rail: boolean; flip: () => void };
}

/**
 * What the jump palette offers besides the fleet's terminals: every view of the app (the board,
 * the timeline, the machines and the factory), so each is a few letters away from anywhere,
 * a phone included, and the app's actions with the key that does each from anywhere.
 */
export function useCommands({ screen, next, onNext, onCreate, onKeys, sidebar }: Options): Command[] {
  const navigate = useNavigate();
  const home = useHome();
  const on = screen === "home" ? home : screen;
  const here = (view: typeof on) => (on === view ? "Here" : undefined);
  const toHome = (view: "board" | "timeline") => {
    setHome(view);
    void navigate({ to: "/" });
  };
  const views: Command[] = [
    {
      key: "board",
      title: "Board",
      group: "Go to",
      about: "Every workspace in lanes by what it needs: you, working, or quiet.",
      also: "home workspaces cards lanes",
      hint: here("board"),
      icon: <BoardIcon />,
      run: () => toHome("board"),
    },
    {
      key: "timeline",
      title: "Timeline",
      group: "Go to",
      about: "Every session's turns on one clock.",
      also: "turns clock",
      hint: here("timeline"),
      icon: <TimelineIcon />,
      run: () => toHome("timeline"),
    },
    {
      key: "machines",
      title: "Machines",
      group: "Go to",
      about: "Each machine's load, memory and disk, its terminals, and where the next task goes.",
      also: "load memory disk",
      hint: here("machines"),
      icon: <MachinesIcon />,
      run: () => void navigate({ to: "/machines" }),
    },
    {
      key: "factory",
      title: "Factory",
      group: "Go to",
      about: "The issues the factory builds: ready, building, held up and finished.",
      also: "issues runs github",
      hint: here("factory"),
      icon: <FactoryIcon className="size-3.5" />,
      run: () => void navigate({ to: "/factory" }),
    },
    {
      key: "settings",
      title: "Settings",
      group: "Go to",
      about: "The factory's, the fleet's and this browser's settings, by category.",
      also: "preferences options configure",
      hint: here("settings"),
      keys: settingsKey,
      icon: <GearIcon className="size-3.5" />,
      run: () => void navigate({ to: "/settings" }),
    },
  ];
  const needs = next ? next.others : 0;
  const actions: Command[] = [
    {
      key: "new-task",
      title: "New task",
      group: "Do",
      about: "Give Claude a task in a workspace and branch of its own.",
      also: "start create workspace",
      icon: <SparkIcon className="size-3.5 text-emerald-400" />,
      run: onCreate,
    },
    ...(next
      ? [
          {
            key: "next",
            title: "Next that needs you",
            group: "Do" as const,
            about: `${next.next.workspace} / ${next.next.name}: ${next.next.fullLabel ?? next.next.label}.`,
            also: "needs you waiting answer",
            hint: `${needs} ${needs === 1 ? "needs" : "need"} you`,
            keys: nextKey,
            icon: <StateGlyph glyph={next.next.glyph} still />,
            run: () => onNext(next.next.terminal),
          },
        ]
      : []),
    ...(sidebar
      ? [
          {
            key: "sidebar",
            title: sidebar.rail ? "Expand sidebar" : "Collapse sidebar",
            group: "Do" as const,
            about: sidebar.rail ? "Show every workspace and terminal by name." : "Fold the workspaces list to a rail.",
            also: "sidebar rail workspaces list",
            keys: railKey,
            icon: <PanelIcon className="size-3.5" />,
            run: sidebar.flip,
          },
        ]
      : []),
    {
      key: "keys",
      title: "Keyboard shortcuts",
      group: "Do",
      about: "Every key Foundry answers, by where it works.",
      also: "keys help",
      keys: "?",
      icon: <KeyIcon />,
      run: onKeys,
    },
  ];
  // After the actions, so a word that names one finds it first.
  const pages: Command[] = categories.map(({ category, label }) => ({
    key: `settings-${category}`,
    title: `Settings: ${label}`,
    group: "Do",
    about: `Open the ${label} page of Settings.`,
    also: "preferences options configure",
    icon: <GearIcon className="size-3.5" />,
    run: () => void navigate({ to: "/settings/$category", params: { category } }),
  }));
  return [...views, ...actions, ...pages];
}

function KeyIcon() {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true">
      <rect x="1.5" y="4" width="13" height="8" rx="1.5" />
      <path d="M4.5 7h.01M7 7h.01M9.5 7h.01M12 7h.01M5 9.5h6" />
    </svg>
  );
}
