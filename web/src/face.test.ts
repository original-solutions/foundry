import { describe, expect, it } from "vitest";
import { faceOf, faces, HUES, letters } from "./face";

const project = (id: string, name: string) => ({ id, name, repo: `/tmp/${name}.git` });

describe("a project's letters", () => {
  it("are the first letters of its first two words, or the first two of its one word", () => {
    expect(letters("billing-api")).toBe("BA");
    expect(letters("docs site")).toBe("DS");
    expect(letters("storefront")).toBe("ST");
    expect(letters("myApp")).toBe("MA");
    expect(letters("api_gateway_v2")).toBe("AG");
  });

  it("make do with what a short or odd name has", () => {
    expect(letters("x")).toBe("X");
    expect(letters("--")).toBe("?");
    expect(letters("")).toBe("?");
    expect(letters("über-cart")).toBe("ÜC");
  });
});

describe("the fleet's faces", () => {
  it("give each project a hue of its own while there are hues to go round", () => {
    const list = Array.from({ length: HUES.length }, (_, i) => project(`id-${i}`, `project ${i}`));
    const hues = list.map((p) => faces(list).get(p.name)?.hue);
    expect(new Set(hues).size).toBe(HUES.length);
    for (const hue of hues) expect(HUES).toContain(hue);
  });

  it("keep every project's face when another project is added", () => {
    const before = [project("a1", "storefront"), project("b2", "billing-api"), project("c3", "docs-site")];
    const after = [...before, project("d4", "mobile-app"), project("e5", "infra")];
    for (const p of before) expect(faces(after).get(p.name)).toEqual(faces(before).get(p.name));
  });

  it("carry the project's letters", () => {
    expect(faces([project("a1", "billing-api")]).get("billing-api")).toEqual({ letters: "BA", hue: expect.any(Number) });
  });

  it("are the same on every machine, since they come from the shared list alone", () => {
    const list = [project("a1", "storefront"), project("b2", "billing-api")];
    expect(faces(list)).toEqual(faces(structuredClone(list)));
  });
});

describe("a face with no list", () => {
  it("is drawn from the name alone, the same each time", () => {
    expect(faceOf("storefront")).toEqual(faceOf("storefront"));
    expect(faceOf("storefront").letters).toBe("ST");
    expect(HUES).toContain(faceOf("storefront").hue);
  });

  it("is the listed one when the list has it", () => {
    const map = faces([project("a1", "storefront")]);
    expect(faceOf("storefront", map)).toBe(map.get("storefront"));
  });
});
