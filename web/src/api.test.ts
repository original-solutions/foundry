import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, createWorkspace, installUpdate, listMachines, sendFile, sessionBriefs, sessionTrail, thisMachine, workspaceChanges, workspaceShipping } from "./api";
import { json } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const answer = (response: Response) => vi.stubGlobal("fetch", vi.fn(async () => response));

const reads = [
  ["briefs", () => sessionBriefs()],
  ["trail", () => sessionTrail("s1")],
  ["changes", () => workspaceChanges("w1")],
  ["shipping", () => workspaceShipping("w1")],
  ["machines", () => listMachines()],
  ["machine", () => thisMachine()],
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
    answer(json(200, { id: "m1" }));
    expect(await thisMachine()).toBe("m1");
  });
});

describe("workspaceShipping", () => {
  it("posts with no body, so another machine's workspace is carried to its owner, and gives back where the branch stands", async () => {
    const shipping = { branch: "fix-login", pushed: true, unpushed: 0, pr: null, unreachable: false };
    const sent: { url: string; method?: string; body?: unknown }[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit = {}) => {
        sent.push({ url, method: init.method, body: init.body });
        return json(200, shipping);
      }),
    );

    expect(await workspaceShipping("w1")).toEqual(shipping);
    expect(sent).toEqual([{ url: "/api/workspaces/w1/pr", method: "POST", body: undefined }]);
  });

  it("refuses any status but 200, as an owner on an older build answers 400", async () => {
    for (const status of [201, 400, 503, 504]) {
      answer(json(status, { error: "no such route" }));
      const error = await workspaceShipping("w1").catch((e: unknown) => e);
      expect(error).toBeInstanceOf(ApiError);
      expect((error as ApiError).status).toBe(status);
    }
  });
});

describe("sendFile", () => {
  it("posts the file itself as the body, its name in the query, and gives back the path it was kept at", async () => {
    const fetch = vi.fn(async () => json(201, { path: "/data/terminals/t1.files/my-shot.png" }));
    vi.stubGlobal("fetch", fetch);
    const file = new File(["png"], "my shot.png", { type: "image/png" });

    expect(await sendFile("t1", file)).toBe("/data/terminals/t1.files/my-shot.png");
    expect(fetch).toHaveBeenCalledWith("/api/terminals/t1/files?name=my%20shot.png", { method: "POST", body: file });
  });

  it("refuses with the daemon's text and its status", async () => {
    answer(json(413, { error: "a file sent to a terminal is at most 25 MB" }));
    const error = await sendFile("t1", new File(["x"], "big.bin")).catch((e: unknown) => e);
    expect((error as ApiError).message).toBe("a file sent to a terminal is at most 25 MB");
    expect((error as ApiError).status).toBe(413);
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

describe("installUpdate", () => {
  it("posts the tag to the machine's update route and gives back the status and body as they came", async () => {
    const fetch = vi.fn(async () => json(409, { error: "v0.9.1 is not ready", ready: "v0.9.2" }));
    vi.stubGlobal("fetch", fetch);
    expect(await installUpdate("machine-2", "v0.9.1")).toEqual({ status: 409, json: { error: "v0.9.1 is not ready", ready: "v0.9.2" } });
    expect(fetch).toHaveBeenCalledWith("/api/machines/machine-2/update", expect.objectContaining({ method: "POST", body: JSON.stringify({ tag: "v0.9.1" }) }));
  });

  it("stops waiting after a minute with no answer, so the call reads as sent with no answer", async () => {
    vi.useFakeTimers();
    try {
      const fetch = vi.fn(
        (_path: string, init: RequestInit) =>
          new Promise<Response>((_answer, fail) => init.signal?.addEventListener("abort", () => fail(new DOMException("timed out", "AbortError")))),
      );
      vi.stubGlobal("fetch", fetch);
      let failed: unknown;
      void installUpdate("machine-2", "v0.9.1").catch((error: unknown) => (failed = error));
      await vi.advanceTimersByTimeAsync(59_999);
      expect(failed).toBeUndefined();
      await vi.advanceTimersByTimeAsync(1);
      expect(failed).toBeInstanceOf(DOMException);
    } finally {
      vi.useRealTimers();
    }
  });

  it("gives back a 202's empty body as {}", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(null, { status: 202 })));
    expect(await installUpdate("machine-2", "v0.9.1")).toEqual({ status: 202, json: {} });
  });
});
