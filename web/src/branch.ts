/** The most bytes a branch name gets, as the daemon cuts it. */
const BRANCH_AT_MOST = 100;
const kept = /^[\p{Alphabetic}\p{N}_.-]$/u;

/**
 * The branch the daemon makes for workspace `name` (its `safe_branch`), so the dialog can show it
 * before anything is made: letters and digits of any script, `_`, `-` and single `.` are kept,
 * anything else becomes `-`, runs of `-` become one, `-` and `.` are cut from both ends, and the
 * result is at most 100 bytes.
 */
export function safeBranch(name: string): string {
  let branch = "";
  for (const ch of name) {
    const c = kept.test(ch) ? ch : "-";
    if ((c === "-" || c === ".") && branch.endsWith(c)) continue;
    branch += c;
  }
  const encoder = new TextEncoder();
  let cut = "";
  let bytes = 0;
  for (const ch of branch) {
    bytes += encoder.encode(ch).length;
    if (bytes > BRANCH_AT_MOST) break;
    cut += ch;
  }
  branch = cut.replace(/^[-.]+|[-.]+$/g, "");
  if (branch.endsWith(".lock")) branch = `${branch.slice(0, -".lock".length)}-lock`;
  if (branch === "") return "workspace";
  if (branch === "HEAD") return "HEAD-workspace";
  return branch;
}

/** Whether two branches are one on a disk that ignores case, which the daemon refuses. */
export function sameBranch(a: string, b: string): boolean {
  const fold = (s: string) => s.toUpperCase().toLowerCase();
  return fold(a) === fold(b);
}
