import { createContext } from "react";
import type { Project } from "./api";

/** How a project is told apart at a glance: two letters on a tile of its own hue. */
export interface Face {
  letters: string;
  /** An oklch hue, one of `HUES`. */
  hue: number;
}

/**
 * The hues a project can take, in oklch degrees. They keep clear of the hues that already mean
 * something on screen (amber needs you, sky works, emerald is ready and the brand, red failed,
 * violet merged), so a project's tile never reads as a state.
 */
export const HUES = [130, 330, 195, 265, 100, 355];

/** The first letters of a name's first two words (dashes, spaces and capitals part them), else its first two letters. */
export function letters(name: string): string {
  const words = name.split(/[^\p{L}\p{N}]+|(?<=\p{Ll})(?=\p{Lu})/u).filter(Boolean);
  const text = words.length >= 2 ? `${[...words[0]][0]}${[...words[1]][0]}` : [...(words[0] ?? "")].slice(0, 2).join("");
  return text ? text.toUpperCase() : "?";
}

/** A small stable number from a string, the same on every machine. */
function hash(text: string): number {
  let h = 2166136261;
  for (const c of text) h = Math.imul(h ^ c.codePointAt(0)!, 16777619);
  return h >>> 0;
}

/**
 * Every project's face, by name. Each project's id picks its hue; one whose hue an earlier
 * project in the shared list holds takes the next free one, so a fleet of up to `HUES.length`
 * projects has no two alike, and adding a project never changes another's.
 */
export function faces(projects: Project[]): Map<string, Face> {
  const out = new Map<string, Face>();
  let taken = new Set<number>();
  for (const p of projects) {
    if (taken.size === HUES.length) taken = new Set();
    let at = hash(p.id) % HUES.length;
    while (taken.has(at)) at = (at + 1) % HUES.length;
    taken.add(at);
    out.set(p.name, { letters: letters(p.name), hue: HUES[at] });
  }
  return out;
}

/** The face of the project named `name`: the listed one, else one drawn from the name alone (a project gone from the list). */
export function faceOf(name: string, listed?: Map<string, Face>): Face {
  return listed?.get(name) ?? { letters: letters(name), hue: HUES[hash(name) % HUES.length] };
}

/** The fleet's faces, given once by the layout from the shared project list. */
export const Faces = createContext<Map<string, Face> | undefined>(undefined);
