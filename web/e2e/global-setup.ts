import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import { chmodSync, closeSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { randomUUID } from "node:crypto";

const root = path.resolve(import.meta.dirname, "../..");
const web = path.join(root, "web");
const bin = (name: string) => path.join(root, "target/debug", name);

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address() as net.AddressInfo;
      server.close(() => resolve(port));
    });
  });
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** An error `until` gives up on at once, rather than taking it for "not up yet". */
class Fatal extends Error {}

async function until<T>(what: string, check: () => Promise<T | undefined | false>, ms = 60_000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    try {
      const found = await check();
      if (found) return found;
    } catch (error) {
      if (error instanceof Fatal) throw error;
      // Not up yet.
    }
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(100);
  }
}

function git(cwd: string, ...args: string[]) {
  execFileSync(
    "git",
    ["-c", "user.name=Foundry E2E", "-c", "user.email=e2e@foundry.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main", ...args],
    { cwd, stdio: "pipe" },
  );
}

function exited(child: ChildProcess): Promise<void> {
  return new Promise((resolve) => {
    if (child.exitCode !== null || child.signalCode !== null) return resolve();
    child.once("exit", () => resolve());
  });
}

async function stop(child: ChildProcess | undefined) {
  if (!child) return;
  child.kill("SIGTERM");
  await Promise.race([exited(child), sleep(10_000)]);
}

/** Builds the daemon, starts it with the fake agent as `claude`, seeds a project, and starts the UI. */
export default async function globalSetup() {
  execFileSync("cargo", ["build", "--locked", "-p", "foundryd", "-p", "foundry-holder", "-p", "foundry-fake-agent"], { cwd: root, stdio: "inherit" });
  execFileSync(path.join(web, "node_modules/.bin/vite"), ["build"], { cwd: web, stdio: "inherit" });

  // The data directory holds Unix sockets, so its path has to stay short.
  const data = mkdtempSync("/tmp/fde2e");
  chmodSync(data, 0o700);
  const home = mkdtempSync(path.join(tmpdir(), "fde2e-home-"));
  const log = path.join(home, "daemon.log");
  const children: ChildProcess[] = [];
  let apiPort = 0;

  const teardown = async () => {
    if (apiPort) {
      // Closing each terminal stops its holder cleanly.
      try {
        const state = (await (await fetch(`http://127.0.0.1:${apiPort}/api/state`)).json()) as { records: { kind: string; deleted: boolean; body: { id: string } }[] };
        for (const r of state.records) {
          if (r.kind === "terminal" && !r.deleted) await fetch(`http://127.0.0.1:${apiPort}/api/terminals/${r.body.id}/close`, { method: "POST" });
        }
      } catch {
        // The daemon is already down.
      }
    }
    for (const child of children.reverse()) await stop(child);
    rmSync(data, { recursive: true, force: true });
    rmSync(home, { recursive: true, force: true });
  };

  try {
    apiPort = await freePort();
    const binDir = path.join(home, "bin");
    mkdirSync(binDir);
    const script = path.join(home, "agent.yaml");
    // The session takes the task as its first prompt, as Claude Code's input box does, and ends its turn.
    writeFileSync(
      script,
      ["- hook: {event: SessionStart}", '- print: "fake agent ready"', "- wait_prompt", "- hook: {event: Stop}", ...Array(50).fill("- wait_input")].join("\n") + "\n",
    );
    writeFileSync(path.join(binDir, "claude"), `#!/bin/sh\nexec ${bin("foundry-fake-agent")} --script ${script} "$@"\n`);
    chmodSync(path.join(binDir, "claude"), 0o755);

    const env: NodeJS.ProcessEnv = {
      ...process.env,
      FOUNDRY_HOME: data,
      FOUNDRY_PORT: String(apiPort),
      FOUNDRY_SECRETS: "file",
      HOME: home,
      SHELL: "/bin/sh",
      FOUNDRY_GATEWAY_URL: "http://gateway.invalid:8317",
      PATH: `${binDir}${path.delimiter}${process.env.PATH ?? ""}`,
    };
    delete env.CLAUDE_CONFIG_DIR;

    execFileSync(bin("foundryd"), ["secret", "set", "gateway"], { env, input: "sk-e2e-not-a-real-key\n", stdio: ["pipe", "pipe", "pipe"] });

    // A project: a local bare repo with one commit on main.
    const remote = path.join(home, "remote");
    mkdirSync(remote);
    git(remote, "init", "-q", "--bare", "remote.git");
    git(remote, "clone", "-q", "remote.git", "src");
    writeFileSync(path.join(remote, "src/README"), "demo\n");
    git(path.join(remote, "src"), "add", ".");
    git(path.join(remote, "src"), "commit", "-q", "-m", "first");
    git(path.join(remote, "src"), "push", "-q", "origin", "main");

    const startDaemon = async () => {
      const fd = openSync(log, "a");
      const child = spawn(bin("foundryd"), ["run"], { env, stdio: ["ignore", "ignore", fd] });
      closeSync(fd);
      children.push(child);
      await until("the daemon to answer", async () => {
        if (child.exitCode !== null) throw new Fatal(`the daemon exited: ${child.exitCode}`);
        return (await fetch(`http://127.0.0.1:${apiPort}/api/state`)).status === 200;
      });
      return child;
    };

    // The first run makes the store and this machine's id; settings are seeded while it is down.
    const first = await startDaemon();
    const me = await until("this machine's sample", async () => {
      const response = await fetch(`http://127.0.0.1:${apiPort}/api/state`);
      const state = (await response.json()) as { records: { kind: string; owner: string }[] };
      return state.records.find((r) => r.kind === "machine_sample")?.owner;
    });
    await stop(first);

    const settings = {
      seq: 1,
      machines: [{ id: me, name: "e2e-machine", address: "127.0.0.1" }],
      priority: [me],
      projects: [{ id: randomUUID(), name: "demo", repo: path.join(remote, "remote.git") }],
      limits: {},
    };
    // No API sets projects yet (the coordinator owns shared settings), so the row is written
    // straight into the store while the daemon is down. The schema belongs to
    // crates/foundryd/src/store/mod.rs; change this with it, or use the settings API once one exists.
    const db = new DatabaseSync(path.join(data, "store.sqlite"));
    db.prepare("INSERT INTO settings (only, seq, settings) VALUES (1, 1, ?) ON CONFLICT (only) DO UPDATE SET seq = excluded.seq, settings = excluded.settings").run(JSON.stringify(settings));
    db.close();
    await startDaemon();

    const uiPort = await freePort();
    const preview = spawn(path.join(web, "node_modules/.bin/vite"), ["preview", "--port", String(uiPort), "--strictPort"], {
      cwd: web,
      env: { ...process.env, FOUNDRY_API: `http://127.0.0.1:${apiPort}` },
      stdio: "ignore",
    });
    children.push(preview);
    await until("the UI to answer", async () => (await fetch(`http://localhost:${uiPort}/`)).ok);

    process.env.FOUNDRY_E2E_URL = `http://localhost:${uiPort}`;
    process.env.FOUNDRY_E2E_API_PORT = String(apiPort);
  } catch (error) {
    let tail = "";
    try {
      tail = readFileSync(log, "utf8").split("\n").slice(-40).join("\n");
    } catch {
      // No log yet.
    }
    await teardown();
    throw new Error(`${error instanceof Error ? error.message : error}\n--- daemon log tail ---\n${tail}`, { cause: error });
  }

  return teardown;
}
