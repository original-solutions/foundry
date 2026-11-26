// @vitest-environment node
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

// The user guide's pages and images, and every other doc page a guide page may link to, keyed by
// their path from the repo root.
const root = join(import.meta.dirname, "..");
const read = (path: string) => readFileSync(join(root, path), "utf8");
const markdown = (dir: string): string[] =>
  readdirSync(join(root, dir), { withFileTypes: true }).flatMap((e) =>
    e.isDirectory() ? markdown(`${dir}${e.name}/`) : e.name.endsWith(".md") ? [`${dir}${e.name}`] : [],
  );
const pages: Record<string, string> = Object.fromEntries(["README.md", ...markdown("docs/")].map((p) => [p, read(p)]));
const images = readdirSync(join(root, "docs/guide/images")).map((name) => `docs/guide/images/${name}`);

const GUIDE = "docs/guide/";
const guidePages = Object.keys(pages).filter((p) => p.startsWith(GUIDE) && !p.slice(GUIDE.length).includes("/"));

/** The path `target` names from the page at `from`, both from the repo root. */
function resolve(from: string, target: string): string {
  const parts = from.split("/").slice(0, -1);
  for (const part of target.split("/")) {
    if (part === "..") parts.pop();
    else if (part !== ".") parts.push(part);
  }
  return parts.join("/");
}

/** The anchor GitHub gives a heading. */
function slug(heading: string): string {
  return heading
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replace(/\s/g, "-");
}

function anchors(markdown: string): Set<string> {
  const found = new Set<string>();
  for (const line of markdown.replace(/```[\s\S]*?```/g, "").split("\n")) {
    const heading = /^#{1,6}\s+(.*)$/.exec(line);
    if (!heading) continue;
    const base = slug(heading[1]);
    let name = base;
    for (let n = 1; found.has(name); n++) name = `${base}-${n}`;
    found.add(name);
  }
  return found;
}

/** Each link and image in `markdown` outside code: whether it is an image, and its target. */
function links(markdown: string): { image: boolean; alt: string; target: string }[] {
  const text = markdown.replace(/```[\s\S]*?```/g, "").replace(/`[^`\n]*`/g, "");
  return [...text.matchAll(/(!?)\[([^\]]*)\]\(([^)\s]+)\)/g)].map((m) => ({ image: m[1] === "!", alt: m[2], target: m[3] }));
}

describe("the user guide", () => {
  it("has pages", () => {
    expect(guidePages.length).toBeGreaterThan(0);
  });

  it.each(guidePages)("%s: every image exists and says what it shows", (page) => {
    for (const { alt, target } of links(pages[page]).filter((l) => l.image)) {
      expect(images, `${page} shows ${target}`).toContain(resolve(page, target));
      expect(alt.trim(), `${page}: ${target} has alt text`).not.toBe("");
    }
  });

  it("uses every image it holds", () => {
    const shown = new Set(guidePages.flatMap((page) => links(pages[page]).filter((l) => l.image).map((l) => resolve(page, l.target))));
    expect(images.filter((image) => !shown.has(image))).toEqual([]);
  });

  it.each(guidePages)("%s: every link to a page or heading resolves", (page) => {
    for (const { target } of links(pages[page]).filter((l) => !l.image && !/^[a-z]+:/.test(l.target))) {
      const [file, anchor] = target.split("#");
      const path = file ? resolve(page, file) : page;
      if (!path.endsWith(".md")) continue;
      expect(Object.keys(pages), `${page} links ${target}`).toContain(path);
      if (anchor) expect([...anchors(pages[path])], `${page} links ${target}`).toContain(anchor);
    }
  });
});
