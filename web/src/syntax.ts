// Colouring code by its language, for reading diffs. A small lexer, not a parser: it knows each
// language's comments, strings, numbers and keywords, and guesses types and calls from how a name
// looks. A comment or string left open at the end of a line stays open on the next, so a block
// comment or a template string is coloured whole. It never changes the text: a line's tokens join
// back to the line.

export type Tone = "keyword" | "string" | "comment" | "number" | "type" | "call" | "heading";

export interface Token {
  text: string;
  tone?: Tone;
}

export interface Lang {
  /** What starts a comment that runs to the end of the line. */
  line?: string[];
  /** What opens and closes a comment that can run across lines. */
  block?: [string, string];
  /** Quotes of strings that end with their line. */
  quotes: string;
  /** Delimiters of strings that can run across lines, longest first. */
  long?: string[];
  keywords: Set<string>;
  /** Words that name a value: true, null, None. */
  literals?: Set<string>;
  /** A name starting with a capital is a type. */
  types?: boolean;
  /** A whole line that heads a section: a Markdown heading, a TOML table. */
  heading?: RegExp;
  /** A config key at the start of a line, before this mark. */
  key?: string;
}

const words = (s: string) => new Set(s.split(" "));

const js: Lang = {
  line: ["//"],
  block: ["/*", "*/"],
  quotes: `"'`,
  long: ["`"],
  keywords: words(
    "abstract as async await break case catch class const continue debugger declare default delete do else enum export extends finally for from function get if implements import in instanceof interface keyof let new of private protected public readonly return satisfies set static super switch this throw try type typeof var void while with yield",
  ),
  literals: words("true false null undefined NaN Infinity"),
  types: true,
};

const languages: Record<string, Lang> = {
  js,
  rust: {
    line: ["//"],
    block: ["/*", "*/"],
    quotes: `"`,
    keywords: words("as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait type unsafe use where while"),
    literals: words("true false"),
    types: true,
  },
  go: {
    line: ["//"],
    block: ["/*", "*/"],
    quotes: `"'`,
    long: ["`"],
    keywords: words("break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var"),
    literals: words("true false nil iota"),
    types: true,
  },
  python: {
    line: ["#"],
    quotes: `"'`,
    long: ['"""', "'''"],
    keywords: words("and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield"),
    literals: words("True False None"),
    types: true,
  },
  shell: {
    line: ["#"],
    quotes: `"'`,
    keywords: words("if then else elif fi for while until do done case esac in function return local export"),
  },
  css: { block: ["/*", "*/"], quotes: `"'`, keywords: new Set() },
  json: { line: ["//"], quotes: `"`, keywords: new Set(), literals: words("true false null") },
  yaml: { line: ["#"], quotes: `"'`, keywords: new Set(), literals: words("true false null"), key: ":" },
  toml: { line: ["#"], quotes: `"'`, keywords: new Set(), literals: words("true false"), key: "=", heading: /^\s*\[.*\]\s*$/ },
  markdown: { quotes: "`", long: ["```"], keywords: new Set(), heading: /^#{1,6}(\s|$)/ },
};

const extensions: Record<string, string> = {
  ts: "js",
  tsx: "js",
  mts: "js",
  cts: "js",
  js: "js",
  jsx: "js",
  mjs: "js",
  cjs: "js",
  rs: "rust",
  go: "go",
  py: "python",
  sh: "shell",
  bash: "shell",
  zsh: "shell",
  css: "css",
  scss: "css",
  json: "json",
  jsonc: "json",
  yml: "yaml",
  yaml: "yaml",
  toml: "toml",
  md: "markdown",
  markdown: "markdown",
  mdx: "markdown",
};

/** The language a file is written in, by its extension; none for one this cannot colour. */
export function languageOf(path: string): Lang | undefined {
  const name = path.slice(path.lastIndexOf("/") + 1);
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return undefined;
  const lang = extensions[name.slice(dot + 1).toLowerCase()];
  return lang ? languages[lang] : undefined;
}

/** A comment or string still open at the end of a line, and what closes it. */
type Open = { tone: "comment" | "string"; close: string } | undefined;

const NAME = /^[A-Za-z_$][\w$]*/;
const NUMBER = /^(?:0[xX][\da-fA-F_]+|0[bB][01_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?)(?:[A-Za-z%][\w%]*)?/;
const CONSTANT = /^[A-Z][A-Z\d_]+$/;

/** Each line's tokens, in order. A comment or long string open at a line's end carries to the next. */
export function lex(lines: string[], lang: Lang): Token[][] {
  let open: Open;
  return lines.map((text) => {
    const out: Token[] = [];
    const push = (part: string, tone?: Tone) => {
      if (!part) return;
      const last = out[out.length - 1];
      if (last && last.tone === tone) last.text += part;
      else out.push({ text: part, tone });
    };
    let i = 0;
    if (!open && lang.heading?.test(text)) {
      push(text, "heading");
      return out;
    }
    if (!open && lang.key) {
      const m = new RegExp(`^(\\s*(?:-\\s+)?)([\\w.-]+)(?=\\s*${lang.key === "=" ? "=" : ":(?:\\s|$)"})`).exec(text);
      if (m) {
        push(m[1]);
        push(m[2], "keyword");
        i = m[0].length;
      }
    }
    while (i < text.length) {
      if (open) {
        const end = closing(text, i, open.close);
        push(text.slice(i, end < 0 ? text.length : end), open.tone);
        if (end < 0) return out;
        i = end;
        open = undefined;
        continue;
      }
      const rest = text.slice(i);
      const block = lang.block && rest.startsWith(lang.block[0]) ? lang.block : undefined;
      if (block) {
        push(block[0], "comment");
        i += block[0].length;
        open = { tone: "comment", close: block[1] };
        continue;
      }
      if (lang.line?.some((m) => rest.startsWith(m))) {
        push(rest, "comment");
        break;
      }
      const long = lang.long?.find((d) => rest.startsWith(d));
      if (long) {
        push(long, "string");
        i += long.length;
        open = { tone: "string", close: long };
        continue;
      }
      const c = text[i];
      // An apostrophe in a word ("don't") opens no string.
      if (lang.quotes.includes(c) && !(c === "'" && /[A-Za-z\d]/.test(text[i - 1] ?? ""))) {
        const end = closing(text, i + 1, c);
        push(text.slice(i, end < 0 ? text.length : end), "string");
        i = end < 0 ? text.length : end;
        continue;
      }
      const before = text[i - 1] ?? "";
      if (/[\w$]/.test(before)) {
        push(c);
        i++;
        continue;
      }
      const number = NUMBER.exec(rest);
      if (number) {
        push(number[0], "number");
        i += number[0].length;
        continue;
      }
      const name = NAME.exec(rest);
      if (name) {
        push(name[0], kind(name[0], text.slice(i + name[0].length), lang));
        i += name[0].length;
        continue;
      }
      push(c);
      i++;
    }
    return out;
  });
}

/** Where a string or comment that closes with `close` ends, searching from `from`, past its closer; -1 if not on this line. */
function closing(text: string, from: number, close: string): number {
  for (let i = from; i < text.length; i++) {
    if (text[i] === "\\" && close !== "*/") {
      i++;
      continue;
    }
    if (text.startsWith(close, i)) return i + close.length;
  }
  return -1;
}

function kind(name: string, after: string, lang: Lang): Tone | undefined {
  if (lang.keywords.has(name)) return "keyword";
  if (lang.literals?.has(name)) return "number";
  if (lang.types && CONSTANT.test(name)) return "number";
  if (/^\s*\(/.test(after) && lang.types) return "call";
  if (lang.types && /^[A-Z]/.test(name)) return "type";
  return undefined;
}
