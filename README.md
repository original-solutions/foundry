# Foundry

A dark software factory that also runs manual sessions. One Rust daemon, `foundryd`, runs on every machine, and the machines find each other over Tailscale.

To use the web UI, start with the [user guide](docs/guide/README.md).

**Status: P1 Core is built.** Joining a fleet, machine limits and removing a machine are built. The acceptance run (plan task 19) is next. To install it, see [docs/install.md](docs/install.md). P1 is specified in [docs/specs/P1-core.md](docs/specs/P1-core.md).

**P2 Factory is being built.** It moves the factory off the Paradev runner and into `foundryd`, as [docs/specs/P2-factory.md](docs/specs/P2-factory.md) specifies. [Set up the factory](docs/install.md#set-up-the-factory) covers a machine's GitHub tokens and the `factory-build` skill, turning a project's factory on, moving a repo from the runner and back, and comparing the two with `retro`.
