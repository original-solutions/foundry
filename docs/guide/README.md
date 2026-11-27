# Foundry user guide

Foundry runs Claude Code sessions on the machines of your fleet, and you watch and steer them from one web page. Every machine runs `foundryd`, which serves the same UI. Open it on any machine and you see the whole fleet.

To install `foundryd` on a machine, add a machine to the fleet, or set up GitHub tokens for the factory, see [Installing foundryd](../install.md).

## Pages

- [Getting started](getting-started.md): open the UI, give Claude its first task, and start more tasks after that.
- [The board and the sidebar](board.md): the home page, the sidebar, the jump palette, and how Foundry tells you a session needs you.
- [Working in a terminal](sessions.md): one workspace's sessions and shells, their changes, their steps, and the keys that drive them.
- [Machines](machines.md): every machine in the fleet, its load and limits, and adding or removing one.
- [The factory](factory.md): the factory's queue, its runs, and turning it on for a project.

## Words this guide uses

- **Workspace**: a folder with a branch of its own, made from a project for one task.
- **Session**: Claude Code running in a workspace. A workspace can hold several.
- **Shell**: a plain command line in a workspace, next to its sessions.
- **Terminal**: a session or a shell.
- **This machine**: the machine whose address you opened the UI at. Some things work only for its workspaces and terminals, and the guide says where.
- **Working**, **Needs you**, **Ready**: the session states you see most. [Session states](sessions.md#session-states) says what each one means, and lists the rarer ones.
