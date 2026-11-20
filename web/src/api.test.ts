import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, createWorkspace, listMachines, sessionBriefs, sessionTrail, workspaceChanges } from "./api";
import { json } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const answer = (response: Response) => vi.stubGlobal("fetch", vi.fn(async () => response));

const reads = [
  ["briefs", () => sessionBriefs()],
  ["trail", () => sessionTrail("s1")],
  ["changes", () => workspaceChanges("w1")],
  ["machines", () => listMachines()],
] as const;

describe("the reads", () => {
  it("each refuse with the daemon's text and its status", async () => {
    for (const [, read] of reads) {
      answer(json(404, { error: "not on this machine" }));
      const error = await read().catch((e: unknown) => e);
      expect(error).toBeInstanceOf(ApiError);
      expect((error as ApiError).message).toBe("not on this machine");
      expect((error as ApiError).status).toBe(404);
    }
  });

  it("each say the status when the daemon's answer is not JSON", async () => {
    for (const [, read] of reads) {
      answer(new Response("Bad Gateway", { status: 502 }));
      const error = await read().catch((e: unknown) => e);
      expect((error as ApiError).message).toBe("request failed (502)");
      expect((error as ApiError).status).toBe(502);
    }
  });

  it("give back what the daemon sent", async () => {
    answer(json(200, { s1: { title: "fix it", now: null } }));
    expect(await sessionBriefs()).toEqual({ s1: { title: "fix it", now: null } });
    answer(json(200, { steps: [{ kind: "done", text: "", at: 1 }] }));
    expect(await sessionTrail("s1")).toEqual([{ kind: "done", text: "", at: 1 }]);
    answer(json(200, { base: "main", commits: [], files: [] }));
    expect(await workspaceChanges("w1")).toEqual({ base: "main", commits: [], files: [] });
  });
});

describe("the machines", () => {
  it("are read with which one is this machine and whether each is online", async () => {
    const machines = { me: "m1", machines: [{ id: "m1", name: "studio", online: true }, { id: "m2", name: "laptop", online: false }] };
    answer(json(200, machines));
    expect(await listMachines()).toEqual(machines);
  });
});

describe("createWorkspace", () => {
  it("names the machine only when one is picked", async () => {
    const sent: unknown[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_url: string, init: RequestInit = {}) => {
        sent.push(JSON.parse(String(init.body)));
        return json(201, { id: "w1" });
      }),
    );
    await createWorkspace({ project: "p1", base: "main", name: "here" });
    await createWorkspace({ project: "p1", base: "main", name: "there", machine: "m2" });
    expect(sent).toEqual([
      { project: "p1", base: "main", name: "here" },
      { project: "p1", base: "main", name: "there", machine: "m2" },
    ]);
  });
});
