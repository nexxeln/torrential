import { useSyncExternalStore } from "react";
import { type Cast, type TvList, tvs } from "../api";

/** How often the list is asked for while nothing plays on a TV, so a cast started on another device shows up. */
const IDLE_MS = 5000;
/** How often while a cast runs, so its time moves with the TV's. */
const LIVE_MS = 1000;

let list: TvList = { tvs: [], casts: [] };
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setTimeout> | undefined;
/** Counts polls, so only the newest one schedules the next. */
let round = 0;

function live(casts: Cast[]): boolean {
  return casts.some((cast) => cast.state !== "ended");
}

function publish(next: TvList): void {
  list = next;
  for (const listener of listeners) listener();
}

/** Asks again after a while: soon while a cast runs. */
function schedule(): void {
  clearTimeout(timer);
  if (listeners.size > 0) timer = setTimeout(poll, live(list.casts) ? LIVE_MS : IDLE_MS);
}

async function poll(): Promise<void> {
  const mine = ++round;
  try {
    const next = await tvs();
    if (mine === round) publish(next);
  } catch {
    // The engine is away. The header says so, and the last list stays.
  }
  if (mine === round) schedule();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  if (listeners.size === 1) void poll();
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) {
      clearTimeout(timer);
      round++;
    }
  };
}

/** The TVs and the casts, as the engine last said. */
export function useTvList(): TvList {
  return useSyncExternalStore(subscribe, () => list);
}

/** Takes a list the engine just answered with, such as after a search, over any older answer on its way. */
export function heard(next: TvList): void {
  round++;
  publish(next);
  schedule();
}

/** Takes a cast the engine just answered with, in place of the TV's last one. */
export function changed(cast: Cast): void {
  heard({ ...list, casts: [...list.casts.filter((other) => other.tv !== cast.tv), cast] });
}
