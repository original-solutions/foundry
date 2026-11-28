import { describe, expect, it } from "vitest";
import { diff, type Field } from "./reviewDiff";

interface Shape {
  paused: boolean;
  model: string;
  max_runs: number | string;
}
const fields: Field<Shape>[] = [
  { key: "paused", label: "Pause claims", show: (v) => (v ? "on" : "off") },
  { key: "model", label: "Model" },
  { key: "max_runs", label: "Runs at once" },
];
const saved: Shape = { paused: false, model: "opus", max_runs: 3 };

describe("diff", () => {
  it("lists only the fields that changed, in the order of the fields", () => {
    expect(diff(saved, { ...saved, max_runs: 4, paused: true }, fields)).toEqual([
      { key: "paused", label: "Pause claims", before: "off", after: "on" },
      { key: "max_runs", label: "Runs at once", before: "3", after: "4" },
    ]);
  });

  it("lists nothing for a draft equal to what was saved", () => {
    expect(diff(saved, { ...saved }, fields)).toEqual([]);
  });

  it("compares numbers as numbers, so 05 is 5", () => {
    expect(diff(saved, { ...saved, max_runs: "03" }, fields)).toEqual([]);
    expect(diff({ ...saved, max_runs: "05" }, { ...saved, max_runs: 5 }, fields)).toEqual([]);
    expect(diff(saved, { ...saved, max_runs: "04" }, fields)).toHaveLength(1);
  });

  it("shows both sides through the field's show, and as text without one", () => {
    const [model] = diff(saved, { ...saved, model: "sonnet" }, fields);
    expect(model).toMatchObject({ before: "opus", after: "sonnet" });
    const [paused] = diff({ ...saved, paused: true }, saved, fields);
    expect(paused).toMatchObject({ before: "on", after: "off" });
  });

  it("counts a blank number as changed, not as zero", () => {
    expect(diff({ ...saved, max_runs: 0 }, { ...saved, max_runs: "" }, fields)).toHaveLength(1);
  });
});
