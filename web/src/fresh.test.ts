import { afterEach, describe, expect, it, vi } from "vitest";
import { reloadIfStale } from "./fresh";
import { json } from "./test-utils";

const page = (script: string) => `<!doctype html><html><head><script type="module" crossorigin src="${script}"></script></head><body></body></html>`;

/** Puts the module script this page was loaded from into the document. */
function loadedFrom(script: string) {
  const tag = document.createElement("script");
  tag.type = "module";
  tag.src = script;
  document.head.append(tag);
}

afterEach(() => {
  document.head.innerHTML = "";
  vi.unstubAllGlobals();
});

describe("reloadIfStale", () => {
  it("reloads when the daemon's page now loads another bundle, as after an upgrade", async () => {
    loadedFrom("/assets/index-old.js");
    const read = vi.fn(async () => new Response(page("/assets/index-new.js"), { status: 200 }));
    vi.stubGlobal("fetch", read);
    const reload = vi.fn();

    await reloadIfStale(reload);

    expect(read).toHaveBeenCalledWith("/", { cache: "no-store" });
    expect(reload).toHaveBeenCalledOnce();
  });

  it("stays when the daemon serves the bundle this page runs", async () => {
    loadedFrom("/assets/index-same.js");
    vi.stubGlobal("fetch", vi.fn(async () => new Response(page("/assets/index-same.js"), { status: 200 })));
    const reload = vi.fn();

    await reloadIfStale(reload);

    expect(reload).not.toHaveBeenCalled();
  });

  it("stays when the page cannot be read or names no bundle, and asks nothing when this page has none", async () => {
    const reload = vi.fn();
    loadedFrom("/assets/index-old.js");
    vi.stubGlobal("fetch", vi.fn(async () => json(500, { error: "down" })));
    await reloadIfStale(reload);
    vi.stubGlobal("fetch", vi.fn(async () => Promise.reject(new TypeError("offline"))));
    await reloadIfStale(reload);
    vi.stubGlobal("fetch", vi.fn(async () => new Response("<html></html>", { status: 200 })));
    await reloadIfStale(reload);
    expect(reload).not.toHaveBeenCalled();

    document.head.innerHTML = "";
    const read = vi.fn();
    vi.stubGlobal("fetch", read);
    await reloadIfStale(reload);
    expect(read).not.toHaveBeenCalled();
  });
});
