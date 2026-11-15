const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

/** A size in at most three figures, in binary units: `512 B`, `1.5 GiB`, `100 MiB`. */
export function bytes(count: number): string {
  let value = count;
  let unit = 0;
  while (Math.round(value) >= 1000 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 99.95 ? 0 : 1;
  return `${value.toFixed(digits)} ${UNITS[unit]}`;
}

export function rate(bytesPerSecond: number): string {
  return `${bytes(bytesPerSecond)}/s`;
}

/** A position in a video: `4:07` or `1:04:07`. */
export function clock(seconds: number): string {
  const whole = Math.max(0, Math.floor(seconds));
  const pad = (n: number) => String(n).padStart(2, "0");
  const [h, m, s] = [Math.floor(whole / 3600), Math.floor((whole % 3600) / 60), whole % 60];
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/** A length of time in its two largest units: `1h 12m`, `5m 3s`, `40s`. */
export function span(seconds: number): string {
  const whole = Math.max(0, Math.round(seconds));
  if (whole >= 86400) return `${Math.floor(whole / 86400)}d ${Math.floor((whole % 86400) / 3600)}h`;
  if (whole >= 3600) return `${Math.floor(whole / 3600)}h ${Math.floor((whole % 3600) / 60)}m`;
  if (whole >= 60) return `${Math.floor(whole / 60)}m ${whole % 60}s`;
  return `${whole}s`;
}

/** How long ago a Unix time was, in one unit: `now` under a minute, then `5h`, `40d`, `2y`. */
export function age(unixSeconds: number, now = Date.now() / 1000): string {
  const elapsed = Math.max(0, now - unixSeconds);
  if (elapsed < 60) return "now";
  if (elapsed < 3600) return `${Math.floor(elapsed / 60)}m`;
  if (elapsed < 86400) return `${Math.floor(elapsed / 3600)}h`;
  if (elapsed < 86400 * 365) return `${Math.floor(elapsed / 86400)}d`;
  return `${Math.floor(elapsed / (86400 * 365))}y`;
}

/** `age` as a phrase: `just now`, or `5m ago`. */
export function ago(unixSeconds: number, now = Date.now() / 1000): string {
  const said = age(unixSeconds, now);
  return said === "now" ? "just now" : `${said} ago`;
}

export function percent(fraction: number): string {
  return `${Math.floor(Math.min(1, Math.max(0, fraction)) * 100)}%`;
}

export function count(n: number, noun: string): string {
  return `${n.toLocaleString("en")} ${noun}${n === 1 ? "" : "s"}`;
}

/** `a, b, and c`. */
export function list(names: string[]): string {
  return new Intl.ListFormat("en", { style: "long", type: "conjunction" }).format(names);
}

/**
 * The engine writes messages for a terminal: lower case, no period. On a page they read as sentences.
 * A message that starts with an ID, such as `tt0063350` or an infohash, keeps it as written.
 */
export function sentence(text: string): string {
  const trimmed = text.trim();
  if (!trimmed) return trimmed;
  const capital = /^\S*\d/.test(trimmed) ? trimmed : trimmed[0]!.toUpperCase() + trimmed.slice(1);
  return /[.!?…]$/.test(capital) ? capital : `${capital}.`;
}

/** What is left of a video stopped at `position` of `duration` seconds: `1h 12m left`. */
export function minutesLeft(position: number, duration: number): string {
  return `${minutes((duration - position) / 60)} left`;
}

/** A running time: `1h 36m`, `2h`, `14m`. */
export function minutes(count: number): string {
  const whole = Math.max(0, Math.round(count));
  const [h, m] = [Math.floor(whole / 60), whole % 60];
  if (h === 0) return `${m}m`;
  return m === 0 ? `${h}h` : `${h}h ${m}m`;
}

const DAY = new Intl.DateTimeFormat("en", { day: "numeric", month: "short", year: "numeric" });

/** A calendar date: `17 Oct 1991`. */
export function day(unixSeconds: number): string {
  const parts = Object.fromEntries(DAY.formatToParts(unixSeconds * 1000).map((part) => [part.type, part.value]));
  return `${parts.day} ${parts.month} ${parts.year}`;
}
