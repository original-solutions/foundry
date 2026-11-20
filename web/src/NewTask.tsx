import { Dialog } from "radix-ui";
import type { Project } from "./api";
import { Composer, useCreateWorkspace } from "./Composer";
import { SparkIcon } from "./FirstPrompt";
import type { WorkspaceView } from "./fleet";

/**
 * The New task dialog: the first run's task box, open over whatever is on screen, so a workspace
 * is made the same way from the sidebar, the jump palette, the board and the first run. The task
 * names the workspace and its branch; with no task, a typed name makes a workspace with Claude
 * idle in it. Start hands the session to `onCreated` and closes, unless the daemon has something
 * to say about the workspace's setup.
 */
export function NewTask({
  projects,
  workspaces,
  open,
  onOpenChange,
  task = "",
  onCreated,
}: {
  projects: Project[];
  /** The workspaces open now, so a name a project already has is never sent. */
  workspaces: WorkspaceView[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The task it opens with, as typed in the jump palette. */
  task?: string;
  /** Called with the new session's id once it has started, so the layout can open it. */
  onCreated: (session: string) => void;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/60 backdrop-blur-[2px]" />
        <Dialog.Content className="fixed inset-x-2 top-2 z-50 mx-auto max-h-[calc(100dvh-1rem)] max-w-2xl overflow-y-auto rounded-2xl border border-neutral-700/70 bg-neutral-950 p-4 text-sm text-neutral-100 shadow-2xl shadow-black/60 sm:top-[12vh] sm:p-6">
          {/* Mounted while open, so each opening starts from what it was given. */}
          <Body projects={projects} workspaces={workspaces} task={task} onCreated={onCreated} close={() => onOpenChange(false)} />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function Body({
  projects,
  workspaces,
  task,
  onCreated,
  close,
}: {
  projects: Project[];
  workspaces: WorkspaceView[];
  task: string;
  onCreated: (session: string) => void;
  close: () => void;
}) {
  const launch = useCreateWorkspace();
  return (
    <div className="space-y-4">
      <div className="space-y-1.5 px-1">
        <Dialog.Title className="flex items-center gap-1.5 text-xs font-semibold tracking-[0.2em] text-neutral-400 uppercase">
          <SparkIcon className="size-3.5 text-emerald-400" />
          New task
        </Dialog.Title>
        <h2 className="text-xl font-semibold tracking-tight text-neutral-50 sm:text-2xl">
          <label htmlFor="new-task">What should Claude work on?</label>
        </h2>
        <Dialog.Description className="text-neutral-400">A branch and a folder of its own on this machine, with Claude Code at work in it.</Dialog.Description>
      </div>
      <Composer
        mode="page"
        id="new-task"
        projects={projects}
        workspaces={workspaces}
        launch={launch}
        initialTask={task}
        onCreated={(session, warning) => {
          onCreated(session);
          if (!warning) close();
        }}
        doneLabel="Close"
        onDone={close}
      />
    </div>
  );
}
