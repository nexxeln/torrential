/** A run of cue text in one style. */
export interface Span {
  text: string;
  italic: boolean;
  bold: boolean;
  underline: boolean;
}

export interface Cue {
  /** Seconds. */
  start: number;
  end: number;
  /** Lines, top to bottom. */
  lines: Span[][];
  /** Where the cue asks to sit. Most ask for nothing and go to the bottom. */
  place: "top" | "middle" | "bottom";
  /** The words alone, to tell cues apart. */
  text: string;
}

const TIMING = /^((?:\d+:)?\d{2}:\d{2}[.,]\d{3})\s+-->\s+((?:\d+:)?\d{2}:\d{2}[.,]\d{3})(.*)$/;
const ENTITIES: Record<string, string> = { amp: "&", lt: "<", gt: ">", nbsp: " ", lrm: "", rlm: "", quot: '"', apos: "'" };
/** ASS alignment codes of the numeric keypad: 7 8 9 are the top row, 4 5 6 the middle. */
const ALIGNMENT = /\{\\an?([1-9])[^}]*\}/;

function seconds(stamp: string): number {
  return stamp
    .replace(",", ".")
    .split(":")
    .reduce((total, part) => total * 60 + Number(part), 0);
}

function place(settings: string, body: string): Cue["place"] {
  const keypad = Number(ALIGNMENT.exec(body)?.[1] ?? 0);
  if (keypad >= 7) return "top";
  if (keypad >= 4) return "middle";
  // WebVTT's `line` is a percentage from the top, or a line number where negative counts from the bottom.
  const line = /\bline:(-?[\d.]+)(%?)/.exec(settings);
  if (!line) return "bottom";
  const value = Number(line[1]);
  if (line[2]) return value < 35 ? "top" : value < 65 ? "middle" : "bottom";
  return value >= 0 && value < 6 ? "top" : "bottom";
}

/** Splits one line into styled runs. Italic, bold, and underline tags style; every other tag is dropped. */
function spans(line: string, open: { i: number; b: number; u: number }): Span[] {
  const runs: Span[] = [];
  for (const part of line.split(/(<[^>]*>)/)) {
    const tag = /^<(\/?)([ibu])\b[^>]*>$/i.exec(part);
    if (tag) {
      const name = tag[2]!.toLowerCase() as "i" | "b" | "u";
      open[name] = Math.max(0, open[name] + (tag[1] ? -1 : 1));
    } else if (!part.startsWith("<") || !part.endsWith(">")) {
      const text = part.replace(/&(\w+);/g, (whole, name: string) => ENTITIES[name] ?? whole);
      if (text) runs.push({ text, italic: open.i > 0, bold: open.b > 0, underline: open.u > 0 });
    }
  }
  return runs;
}

function cue(block: string): Cue | null {
  const rows = block.split("\n");
  const at = rows.findIndex((row) => row.includes("-->"));
  const timing = at === -1 ? null : TIMING.exec(rows[at]!.trim());
  if (!timing) return null;
  const body = rows.slice(at + 1).join("\n");
  // ASS override blocks leak through converters: {\an8} places the cue, {\i1} and the rest are noise here.
  const clean = body.replace(/\{\\[^}]*\}/g, "").replace(/\\N/g, "\n");
  const open = { i: 0, b: 0, u: 0 };
  const lines = clean
    .split("\n")
    .map((row) => spans(row.trim(), open))
    .filter((row) => row.length > 0);
  if (lines.length === 0) return null;
  return {
    start: seconds(timing[1]!),
    end: seconds(timing[2]!),
    lines,
    place: place(timing[3]!, body),
    text: lines.map((row) => row.map((run) => run.text).join("")).join("\n"),
  };
}

/** Reads WebVTT as it arrives. Blocks end at a blank line, so the last one waits for `finish`. */
export class VttParser {
  private rest = "";

  /** The cues completed by `chunk`. */
  feed(chunk: string): Cue[] {
    this.rest += chunk.replace(/\r\n?/g, "\n");
    const cues: Cue[] = [];
    for (let end = this.rest.indexOf("\n\n"); end !== -1; end = this.rest.indexOf("\n\n")) {
      const parsed = cue(this.rest.slice(0, end));
      if (parsed) cues.push(parsed);
      this.rest = this.rest.slice(end + 2);
    }
    return cues;
  }

  finish(): Cue[] {
    const parsed = cue(this.rest);
    this.rest = "";
    return parsed ? [parsed] : [];
  }
}
