import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, sendFile, sessionBriefs, sessionTrail, thisMachine, workspaceChanges } from "./api";
import { json } from "./test-utils";

afterEach(() => vi.unstubAllGlobals());

const answer = (response: Response) => vi.stubGlobal("fetch", vi.fn(async () => response));

const reads = [
  ["briefs", () => sessionBriefs()],
  ["trail", () => sessionTrail("s1")],
  ["changes", () => workspaceChanges("w1")],
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
