/** A line that starts an item of a list: a dash, star, bullet or number, then space. */
export const ITEM = /^(?:[-*•]|\d+[.)])\s+/u;

/**
 * The items of a task written as a list, or `undefined` when it is not one. Every line that is
 * not blank starts an item with a dash, star, bullet or number, or is indented under the item
 * above it, which keeps it; an item is its text with its mark taken off. Fewer than two items is
 * no list, so a task that only starts with a dash stays one task.
 */
export function listOf(task: string): string[] | undefined {
  const items: string[] = [];
  for (const line of task.split("\n")) {
    if (line.trim() === "") continue;
    const text = line.trim().replace(ITEM, "");
    if (text !== line.trim()) items.push(text);
    else if (/^\s/u.test(line) && items.length > 0) items[items.length - 1] += `\n${text}`;
    else return undefined;
  }
  return items.length >= 2 ? items : undefined;
}
