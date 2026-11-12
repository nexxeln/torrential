const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

/** `1.8 GiB`, in the binary units the CLI prints. */
export function bytes(count: number): string {
  let value = count;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : value >= 10 ? 1 : 2;
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

/** How long ago a Unix time was, in one unit: `5h`, `40d`, `2y`. */
export function age(unixSeconds: number, now = Date.now() / 1000): string {
  const elapsed = Math.max(0, now - unixSeconds);
  if (elapsed < 3600) return `${Math.max(1, Math.floor(elapsed / 60))}m`;
  if (elapsed < 86400) return `${Math.floor(elapsed / 3600)}h`;
  if (elapsed < 86400 * 365) return `${Math.floor(elapsed / 86400)}d`;
  return `${Math.floor(elapsed / (86400 * 365))}y`;
}

export function percent(fraction: number): string {
  return `${Math.floor(Math.min(1, Math.max(0, fraction)) * 100)}%`;
}

export function count(n: number, noun: string): string {
  return `${n.toLocaleString("en")} ${noun}${n === 1 ? "" : "s"}`;
}

/** The engine writes messages for a terminal: lower case, no period. On a page they read as sentences. */
export function sentence(text: string): string {
  const trimmed = text.trim();
  if (!trimmed) return trimmed;
  const capital = trimmed[0]!.toUpperCase() + trimmed.slice(1);
  return /[.!?…]$/.test(capital) ? capital : `${capital}.`;
}
