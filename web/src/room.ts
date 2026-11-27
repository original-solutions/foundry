import type { Limits, MachineSample } from "./api";
import type { MachineView } from "./fleet";

/**
 * Whether a machine would take a task Foundry chooses the machine for: the one it would pick, one
 * with room, one at or over a limit (and which), one offline, or one it cannot say of (no numbers
 * yet, or not known to be online).
 */
export type Room = { kind: "pick" } | { kind: "room" } | { kind: "full"; over: string[] } | { kind: "offline" } | { kind: "unknown" };

/** `text`, a number with decimals, without trailing zeros but with at least one decimal. */
const trimmed = (text: string) => {
  const cut = text.replace(/0+$/, "");
  return cut.endsWith(".") ? `${cut}0` : cut;
};

/** `value` and `limit` at the fewest decimals, two at least, that print them apart, as `machine::apart` does. */
function apart(value: number, limit: number): [string, string] {
  for (let places = 2; places <= 17; places++) {
    const shown = [value.toFixed(places), limit.toFixed(places)];
    if (shown[0] !== shown[1]) return [trimmed(shown[0]), trimmed(shown[1])];
  }
  const equal = trimmed(value.toFixed(2));
  return [equal, equal];
}

/**
 * Every limit `sample` is at or over, in the order sessions, load, disk, worded as the daemon's
 * placement reasons are (`machine::over_limits`), such as `load 1.8 over limit 1.0`.
 */
export function overLimits(sample: MachineSample, limits: Limits): string[] {
  const over: string[] = [];
  if (sample.sessions >= limits.max_sessions) {
    over.push(`sessions ${sample.sessions} ${sample.sessions === limits.max_sessions ? "at" : "over"} limit ${limits.max_sessions}`);
  }
  if (sample.load_per_core >= limits.max_load_per_core) {
    const [value, limit] = apart(sample.load_per_core, limits.max_load_per_core);
    over.push(`load ${value} ${sample.load_per_core === limits.max_load_per_core ? "at" : "over"} limit ${limit}`);
  }
  if (sample.free_disk < limits.min_free_disk) {
    const [value, limit] = apart(sample.free_disk / 1e9, limits.min_free_disk / 1e9);
    over.push(`free disk ${value} GB under limit ${limit} GB`);
  }
  return over;
}

/**
 * Where a task Foundry chooses the machine for would go now, by the rule placement uses
 * (`placement::place`) on each machine's newest sample: among the online machines under every
 * limit they have, the lowest load per core, then the fewest sessions, then the lower id. Each
 * machine's `room`, and, as the daemon gives them when none qualifies, one reason per machine
 * that sent numbers and was passed over. Creates still on their way to a machine are not
 * counted, so this is a preview: the daemon decides when the task is sent. The daemon's tests
 * and this copy's run the same cases (`placement_cases.json` here), so the two rules
 * cannot change apart without a test failing.
 */
export function rooms(machines: MachineView[]): { pick?: string; room: Map<string, Room>; reasons: string[] } {
  const room = new Map<string, Room>();
  const reasons: string[] = [];
  let best: MachineView | undefined;
  for (const machine of machines) {
    const { sample } = machine;
    if (!sample || machine.status === "unknown") {
      room.set(machine.id, { kind: "unknown" });
      continue;
    }
    if (machine.status === "offline") {
      room.set(machine.id, { kind: "offline" });
      reasons.push(`${machine.name}: offline`);
      continue;
    }
    const over = machine.limits ? overLimits(sample, machine.limits) : [];
    if (over.length > 0) {
      room.set(machine.id, { kind: "full", over });
      reasons.push(`${machine.name}: ${over.join(", ")}`);
      continue;
    }
    room.set(machine.id, { kind: "room" });
    if (!best || roomier(machine, sample, best)) best = machine;
  }
  if (best) room.set(best.id, { kind: "pick" });
  return { pick: best?.id, room, reasons };
}

/** Whether `a` has more headroom than `b`: less load per core, then fewer sessions, then the lower id. */
function roomier(a: MachineView, sample: MachineSample, b: MachineView): boolean {
  const other = b.sample!;
  if (sample.load_per_core !== other.load_per_core) return sample.load_per_core < other.load_per_core;
  if (sample.sessions !== other.sessions) return sample.sessions < other.sessions;
  return a.id < b.id;
}
