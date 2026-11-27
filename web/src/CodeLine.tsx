import type { Piece } from "./paint";
import type { Tone } from "./syntax";

/** A colour per kind of token, close to the editors people read code in; the light theme mirrors each. */
const tones: Record<Tone, string> = {
  keyword: "text-violet-300",
  call: "text-amber-200",
  type: "text-sky-300",
  string: "text-orange-300",
  number: "text-emerald-300",
  comment: "text-neutral-500 italic",
  heading: "text-sky-300 font-semibold",
};

/** The words a change changed, a stronger shade of the line's own. */
const changed = { added: "bg-emerald-400/25", removed: "bg-red-400/30" };

/** A diff line's text as its pieces: coloured as code, with the words that changed marked. */
export function CodeLine({ pieces, kind }: { pieces: Piece[]; kind: "added" | "removed" | "same" }) {
  return pieces.map((p, i) => {
    const tone = p.tone ? tones[p.tone] : undefined;
    return p.changed && kind !== "same" ? (
      <mark key={i} data-tone={p.tone} className={`rounded-[2px] ${changed[kind]} ${tone ?? "text-inherit"}`}>
        {p.text}
      </mark>
    ) : (
      <span key={i} data-tone={p.tone} className={tone}>
        {p.text}
      </span>
    );
  });
}
