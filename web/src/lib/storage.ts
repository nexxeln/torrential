// What the app remembers in this browser: where each video stopped, and how the player was set.

export interface Position {
  id: string;
  file: number;
  /** The release name and the file's path, to list it without asking the engine. */
  name: string;
  path: string;
  /** Seconds. */
  time: number;
  duration: number;
  finished: boolean;
  /** Unix milliseconds of the last update. */
  at: number;
}

/** How subtitle text is set off from the picture behind it. */
export type SubtitleBacking = "shadow" | "outline" | "box";

export interface SubtitleStyle {
  /** A multiple of the normal size. */
  scale: number;
  backing: SubtitleBacking;
  /** How far the lines sit above the bottom edge, as a percentage of the picture's height. */
  lift: number;
}

export interface Preferences {
  volume: number;
  muted: boolean;
  speed: number;
  /** An ISO 639-1 code such as `en`, or null for subtitles off. */
  subtitles: string | null;
  subtitleStyle: SubtitleStyle;
  /** The time readout counts down to the end instead of showing the length. */
  showRemaining: boolean;
}

const POSITIONS = "torrential:positions";
const PREFERENCES = "torrential:preferences";
const OFFSETS = "torrential:subtitle-offsets";
const KEPT = 200;
export const SUBTITLE_STYLE: SubtitleStyle = { scale: 1, backing: "shadow", lift: 7 };
const DEFAULTS: Preferences = {
  volume: 1,
  muted: false,
  speed: 1,
  subtitles: null,
  subtitleStyle: SUBTITLE_STYLE,
  showRemaining: false,
};

function read<T>(key: string, fallback: T): T {
  try {
    const text = localStorage.getItem(key);
    return text ? (JSON.parse(text) as T) : fallback;
  } catch {
    return fallback;
  }
}

function write(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Private windows and full storage refuse writes. Playback goes on without memory.
  }
}

const key = (id: string, file: number) => `${id}:${file}`;

export function position(id: string, file: number): Position | null {
  return read<Record<string, Position>>(POSITIONS, {})[key(id, file)] ?? null;
}

/** Newest first. */
export function positions(): Position[] {
  return Object.values(read<Record<string, Position>>(POSITIONS, {})).sort((a, b) => b.at - a.at);
}

/** The last seconds of a video are credits, so stopping there counts as finishing it. */
export function isFinished(time: number, duration: number): boolean {
  return duration > 0 && duration - time <= Math.max(20, duration * 0.03);
}

export function savePosition(entry: Omit<Position, "finished" | "at">): void {
  const all = read<Record<string, Position>>(POSITIONS, {});
  all[key(entry.id, entry.file)] = { ...entry, finished: isFinished(entry.time, entry.duration), at: Date.now() };
  const newest = Object.entries(all)
    .sort(([, a], [, b]) => b.at - a.at)
    .slice(0, KEPT);
  write(POSITIONS, Object.fromEntries(newest));
}

export function forgetPosition(id: string, file: number): void {
  const all = read<Record<string, Position>>(POSITIONS, {});
  delete all[key(id, file)];
  write(POSITIONS, all);
}

export function preferences(): Preferences {
  const stored = read<Partial<Preferences>>(PREFERENCES, {});
  return { ...DEFAULTS, ...stored, subtitleStyle: { ...SUBTITLE_STYLE, ...stored.subtitleStyle } };
}

export function savePreferences(change: Partial<Preferences>): void {
  write(PREFERENCES, { ...preferences(), ...change });
}

/**
 * How many seconds later than written a subtitle track shows, for one video file. Negative is
 * earlier. `track` is the track's key in the subtitle menu.
 */
export function subtitleOffset(id: string, file: number, track: string): number {
  return read<Record<string, number>>(OFFSETS, {})[`${id}:${file}:${track}`] ?? 0;
}

export function saveSubtitleOffset(id: string, file: number, track: string, seconds: number): void {
  const all = read<Record<string, number>>(OFFSETS, {});
  const name = `${id}:${file}:${track}`;
  // Insertion order is age order, so re-adding keeps the newest KEPT.
  delete all[name];
  if (seconds !== 0) all[name] = seconds;
  write(OFFSETS, Object.fromEntries(Object.entries(all).slice(-KEPT)));
}
