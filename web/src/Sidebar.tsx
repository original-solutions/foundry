import { useMutation } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ArchiveButton, button } from "./ArchiveButton";
import { CreateWorkspace } from "./CreateWorkspace";
import { startSession, startShell, type Project } from "./api";
import { machineNumbers, type MachineView, type WorkspaceView } from "./fleet";
import { sessionLabel, terminalLabel } from "./labels";

const statusColour = { online: "text-emerald-400", offline: "text-red-400", unknown: "text-neutral-400" };

interface Props {
  machines: MachineView[];
  projects: Project[];
  /** The terminal open in the main pane. */
  selected?: string;
  connected: boolean;
}

export function Sidebar({ machines, projects, selected, connected }: Props) {
  return (
    <nav className="flex h-full w-80 shrink-0 flex-col gap-4 overflow-y-auto bg-neutral-900 p-3 text-sm text-neutral-100">
      <div className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Foundry</h1>
        <CreateWorkspace projects={projects} />
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
            <Workspace key={workspace.id} workspace={workspace} selected={selected} />
          ))}
        </section>
      ))}
    </nav>
  );
}

function Workspace({ workspace, selected }: { workspace: WorkspaceView; selected?: string }) {
  const session = useMutation({ mutationFn: () => startSession(workspace.id, "opus") });
  const shell = useMutation({ mutationFn: () => startShell(workspace.id) });
  const error = session.error ?? shell.error;
  return (
    <div role="group" aria-label={workspace.name} className="space-y-1 rounded bg-neutral-800/50 p-2">
      <div className="font-medium">{workspace.name}</div>
      <div className="text-xs text-neutral-400">
        {workspace.branch} · {workspace.project}
      </div>
      <div className="flex flex-wrap gap-1">
        <button type="button" className={button} onClick={() => session.mutate()} disabled={session.isPending}>
          New session
        </button>
        <button type="button" className={button} onClick={() => shell.mutate()} disabled={shell.isPending}>
          New shell
        </button>
        <ArchiveButton workspace={workspace.id} name={workspace.name} />
      </div>
      {error && (
        <p role="alert" className="text-xs text-red-400">
          {error.message}
        </p>
      )}
      <ul>
        {workspace.sessions.map((s) => (
          <Item key={s.id} terminal={s.terminal} selected={selected} name={s.model} label={sessionLabel[s.state]} />
        ))}
        {workspace.shells.map((t) => (
          <Item key={t.id} terminal={t.id} selected={selected} name="shell" label={terminalLabel(t.state, t.exitCode)} />
        ))}
      </ul>
    </div>
  );
}

function Item({ terminal, selected, name, label }: { terminal: string; selected?: string; name: string; label: string }) {
  const current = terminal === selected;
  return (
    <li>
      <Link
        to="/terminals/$terminalId"
        params={{ terminalId: terminal }}
        aria-current={current ? "true" : undefined}
        className={`flex justify-between rounded px-2 py-1 hover:bg-neutral-700 ${current ? "bg-neutral-700" : ""}`}
      >
        <span>{name}</span> <span className="text-neutral-300">{label}</span>
      </Link>
    </li>
  );
}
