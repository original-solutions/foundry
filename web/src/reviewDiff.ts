/** One changed field as the review bar lists it: what it was and what it will be. */
export interface Change {
  key: string;
  label: string;
  before: string;
  after: string;
}

/** A save a page was refused: the daemon's reason, and the field it concerns when it names one. */
export interface Refusal {
  key?: string;
  message: string;
}

/** A field of `T` the review bar compares, with its label and, when its value is not plain text, how to show it. */
export interface Field<T> {
  key: keyof T & string;
  label: string;
  show?: (value: T[keyof T]) => string;
}

/** A value as a number when it is one or text of one; blank is not a number. */
function asNumber(value: unknown): number {
  if (typeof value === "number") return value;
  if (typeof value === "string" && value.trim() !== "") return Number(value);
  return NaN;
}

/** Whether two values are one: numbers by their number, so "05" is 5, anything else as it is. */
function same(a: unknown, b: unknown): boolean {
  if (typeof a === "number" || typeof b === "number") {
    const x = asNumber(a);
    const y = asNumber(b);
    return !Number.isNaN(x) && x === y;
  }
  return a === b;
}

/** The fields of `fields` whose value in `draft` differs from `saved`, in the order of `fields`. */
export function diff<T>(saved: T, draft: T, fields: Field<T>[]): Change[] {
  const changes: Change[] = [];
  for (const { key, label, show } of fields) {
    const before = saved[key];
    const after = draft[key];
    if (same(before, after)) continue;
    const text = show ?? ((value: T[keyof T]) => String(value));
    changes.push({ key, label, before: text(before), after: text(after) });
  }
  return changes;
}
