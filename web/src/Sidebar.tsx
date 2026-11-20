import { useMutation } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ArchiveButton } from "./ArchiveButton";
import { button } from "./Modal";
import { CreateWorkspace } from "./CreateWorkspace";
import { jumpKey, SearchIcon } from "./JumpPalette";
import { NewSession } from "./NewSession";
import { startShell, type Project } from "./api";
import { machineNumbers, type MachineView, type WorkspaceView } from "./fleet";
import { sessionLabel, sessionTone, terminalLabel, terminalTone, type Tone } from "./labels";
import { StateDot } from "./StateDot";

const statusColour = { online: "text-emerald-400", offline: "text-red-400", unknown: "text-neutral-400" };

interface Props {
  machines: MachineView[];
  projects: Project[];
  /** The terminal open in the main pane. */
  selected?: string;
  connected: boolean;
  /** Called when a terminal is picked, so a drawer can close. */
  onNavigate?: () => void;
  /** Given while the sidebar is a drawer over the terminal: its close button. */
  onClose?: () => void;
  /** Opens the jump palette. */
  onJump?: () => void;
}

export function Sidebar({ machines, projects, selected, connected, onNavigate, onClose, onJump }: Props) {
  return (
    <nav id="sidebar" className="flex h-full w-full shrink-0 flex-col gap-4 overflow-y-auto bg-neutral-900 p-3 text-sm text-neutral-100 md:w-80">
      <div className="flex items-center justify-between gap-2">
        <h1 className="text-lg font-semibold">Foundry</h1>
        <div className="flex items-center gap-2">
          {onJump && (
            <button type="button" aria-label="Jump to" title={`Jump to (${jumpKey})`} onClick={onJump} className="rounded p-1 text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200">
              <SearchIcon className="size-4" />
            </button>
          )}
          <CreateWorkspace projects={projects} />
          {onClose && (
            <button type="button" aria-label="Close workspaces" onClick={onClose} className="rounded p-1 text-neutral-300 hover:bg-neutral-800 md:hidden">
              <svg viewBox="0 0 16 16" className="size-4" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
                <path d="M4 4l8 8M12 4l-8 8" strokeLinecap="round" />
              </svg>
            </button>
          )}
        </div>
      </div>
      {!connected && <p className="text-xs text-amber-400">Disconnected, reconnecting</p>}
      {machines.map((machine) => (
        <section key={machine.id} aria-label={machine.name} className="space-y-2">
          <div>
            <div className="flex items-baseline justify-between">
              <h2 className="font-semibold">{machine.name}</h2>
              <span className={`text-xs ${statusColour[machine.status]}`}>{machine.status}</span>
            </div>
            <div className="text-xs text-neutral-400">{machineNumbers(machine.sample)}</div>
          </div>
          {machine.workspaces.map((workspace) => (
            <Workspace key={workspace.id} workspace={workspace} selected={selected} onNavigate={onNavigate} />
          ))}
        </section>
      ))}
    </nav>
  );
}

function Workspace({ workspace, selected, onNavigate }: { workspace: WorkspaceView; selected?: string; onNavigate?: () => void }) {
  const shell = useMutation({ mutationFn: () => startShell(workspace.id) });
  return (
    <div role="group" aria-label={workspace.name} className="space-y-1 rounded bg-neutral-800/50 p-2">
      <div className="font-medium">{workspace.name}</div>
      <div className="text-xs text-neutral-400">
        {workspace.branch} · {workspace.project}
      </div>
      <div className="flex flex-wrap gap-1">
        <NewSession workspace={workspace.id} name={workspace.name} />
        <button type="button" className={button} onClick={() => shell.mutate()} disabled={shell.isPending}>
          New shell
        </button>
        <ArchiveButton workspace={workspace.id} name={workspace.name} />
      </div>
      {shell.error && (
        <p role="alert" className="text-xs text-red-400">
          {shell.error.message}
        </p>
      )}
      <ul>
        {workspace.sessions.map((s) => (
          <Item key={s.id} terminal={s.terminal} selected={selected} name={s.model} label={sessionLabel[s.state]} tone={sessionTone[s.state]} onNavigate={onNavigate} />
        ))}
        {workspace.shells.map((t) => (
          <Item
            key={t.id}
            terminal={t.id}
            selected={selected}
            name="shell"
            label={terminalLabel(t.state, t.exitCode)}
            tone={terminalTone(t.state, t.exitCode)}
            onNavigate={onNavigate}
          />
        ))}
      </ul>
    </div>
  );
}

interface ItemProps {
  terminal: string;
  selected?: string;
  name: string;
  label: string;
  tone: Tone;
  onNavigate?: () => void;
}

function Item({ terminal, selected, name, label, tone, onNavigate }: ItemProps) {
  const current = terminal === selected;
  return (
    <li>
      <Link
        to="/terminals/$terminalId"
        params={{ terminalId: terminal }}
        aria-current={current ? "true" : undefined}
        onClick={onNavigate}
        className={`flex items-center gap-2 rounded px-2 py-1.5 hover:bg-neutral-700 md:py-1 ${current ? "bg-neutral-700" : ""}`}
      >
        <StateDot tone={tone} label={label} />
        <span className="flex-1">{name}</span> <span className="text-neutral-300">{label}</span>
      </Link>
    </li>
  );
}
