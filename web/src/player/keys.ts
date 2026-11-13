import { isBack, isTv } from "../lib/tv";

/** What a key press asks of the player. */
export type Command =
  | { kind: "toggle" }
  | { kind: "seekBy"; seconds: number }
  /** A fraction of the video's length. */
  | { kind: "seekTo"; fraction: number }
  | { kind: "step"; direction: 1 | -1 }
  | { kind: "chapter"; direction: 1 | -1 }
  | { kind: "file"; direction: 1 | -1 }
  | { kind: "volumeBy"; amount: number }
  | { kind: "mute" }
  | { kind: "speedBy"; steps: number }
  | { kind: "fullscreen" }
  | { kind: "subtitles" }
  /** Seconds later. Negative is earlier. */
  | { kind: "subtitleDelay"; seconds: number }
  | { kind: "audio" }
  | { kind: "stats" }
  | { kind: "shortcuts" }
  /** Shows the controls with focus on the timeline, for a remote that has no other way to reach them. */
  | { kind: "controls" }
  /** Escape or a remote's Back: close what is open, or go back. */
  | { kind: "leave" };

type Key = { keys: string; does: string };

/** The shortcuts, as the sheet behind `?` lists them. */
export const SHORTCUTS: { title: string; keys: Key[] }[] = [
  {
    title: "Playback",
    keys: [
      { keys: "Space  K", does: "Play or pause" },
      { keys: "←  →", does: "Back or forward 5 seconds" },
      { keys: "J  L", does: "Back or forward 10 seconds" },
      { keys: "Shift ←  →", does: "Back or forward 30 seconds" },
      { keys: "0 – 9", does: "Jump to 0% – 90%" },
      { keys: "Home  End", does: "Jump to the start or the end" },
      { keys: ",  .", does: "One frame back or forward, while paused" },
      { keys: "<  >", does: "Slower or faster" },
      { keys: "Page Up  Page Down", does: "Previous or next chapter" },
      { keys: "P  N", does: "Previous or next video in the torrent" },
    ],
  },
  {
    title: "Sound and subtitles",
    keys: [
      { keys: "↑  ↓", does: "Volume up or down" },
      { keys: "M", does: "Mute" },
      { keys: "A", does: "Next audio track" },
      { keys: "C", does: "Subtitles on or off" },
      { keys: "G  H", does: "Subtitles 0.1 s earlier or later" },
      { keys: "Shift G  H", does: "Subtitles 0.5 s earlier or later" },
    ],
  },
  {
    title: "Window",
    keys: [
      { keys: "F", does: "Fullscreen" },
      { keys: "I", does: "Playback details" },
      { keys: "?", does: "This list" },
      { keys: "Esc", does: "Close, or go back" },
    ],
  },
];

const LETTERS: Record<string, Command | ((shift: boolean) => Command)> = {
  k: { kind: "toggle" },
  j: { kind: "seekBy", seconds: -10 },
  l: { kind: "seekBy", seconds: 10 },
  m: { kind: "mute" },
  f: { kind: "fullscreen" },
  c: { kind: "subtitles" },
  a: { kind: "audio" },
  i: { kind: "stats" },
  n: { kind: "file", direction: 1 },
  p: { kind: "file", direction: -1 },
  g: (shift) => ({ kind: "subtitleDelay", seconds: shift ? -0.5 : -0.1 }),
  h: (shift) => ({ kind: "subtitleDelay", seconds: shift ? 0.5 : 0.1 }),
};

const NAMED: Record<string, Command | ((shift: boolean) => Command)> = {
  " ": { kind: "toggle" },
  ArrowLeft: (shift) => ({ kind: "seekBy", seconds: shift ? -30 : -5 }),
  ArrowRight: (shift) => ({ kind: "seekBy", seconds: shift ? 30 : 5 }),
  ArrowUp: { kind: "volumeBy", amount: 0.05 },
  ArrowDown: { kind: "volumeBy", amount: -0.05 },
  Home: { kind: "seekTo", fraction: 0 },
  End: { kind: "seekTo", fraction: 1 },
  PageUp: { kind: "chapter", direction: -1 },
  PageDown: { kind: "chapter", direction: 1 },
  ",": { kind: "step", direction: -1 },
  ".": { kind: "step", direction: 1 },
  "<": { kind: "speedBy", steps: -1 },
  ">": { kind: "speedBy", steps: 1 },
  "?": { kind: "shortcuts" },
};

/** On a TV, Enter plays or pauses, and Up and Down bring up the controls, since the TV's own remote sets the volume. */
const REMOTE: Record<string, Command> = {
  Enter: { kind: "toggle" },
  ArrowUp: { kind: "controls" },
  ArrowDown: { kind: "controls" },
};

/**
 * The command for a key press, or null when the key belongs to the control that has focus.
 * Symbols are read as typed, wherever the layout puts them. Letters and digits fall back to the
 * key's position, so they work on layouts that type other scripts or need Shift for digits.
 */
export function command(event: KeyboardEvent): Command | null {
  if (event.metaKey || event.ctrlKey || event.altKey) return null;
  if (isBack(event)) return { kind: "leave" };
  const target = event.target as Element;
  // Menus and form controls use arrows, Home, End, and typing themselves.
  if (target.closest("[role=menu], input, select, textarea, [contenteditable]")) return null;
  if ((event.key === " " || event.key === "Enter") && target.closest("button, a")) return null;

  const { key, code, shiftKey: shift } = event;
  const remote = isTv() ? REMOTE[key] : undefined;
  if (remote) return remote;
  const digit = /^[0-9]$/.test(key) ? key : !shift && /^Digit[0-9]$/.test(code) && key.length === 1 && !NAMED[key] ? code.slice(5) : null;
  if (digit) return { kind: "seekTo", fraction: Number(digit) / 10 };
  const letter = /^[a-z]$/i.test(key) ? key.toLowerCase() : /^Key[A-Z]$/.test(code) && key.length === 1 ? code.slice(3).toLowerCase() : null;
  const found = letter ? LETTERS[letter] : NAMED[key];
  return typeof found === "function" ? found(shift) : (found ?? null);
}
