import type { ApiError } from "../api";

/** Whether the playing source still serves the playhead. Only title sessions leave `watching`. */
export type Recovery =
  | { kind: "watching" }
  /** Waiting, under 1 s buffered ahead, and no bytes arriving, since `since` (milliseconds). */
  | { kind: "suspect"; since: number }
  /** Asked the engine for the next source, at `at` seconds into the video. The current source keeps playing. */
  | { kind: "switching"; at: number }
  /** No other source plays. Keep waiting on this one, and say so. */
  | { kind: "exhausted"; error: ApiError };

export type RecoveryEvent =
  /** What the player sees, about once a second. */
  | {
      type: "sample";
      /** Milliseconds, from any steady clock. */
      now: number;
      /** The playhead, in seconds. */
      time: number;
      /** The video waits for data to go on. */
      waiting: boolean;
      /** Seconds buffered ahead of the playhead. */
      ahead: number;
      /** The download receives bytes, or has them all. */
      receiving: boolean;
      /** The download stopped with an error. */
      failed: boolean;
    }
  /** Another source plays, from the playhead. */
  | { type: "playing" }
  /** The engine found no other source that plays. */
  | { type: "failed"; error: ApiError }
  /** The person asks to look for another source again. */
  | { type: "retry"; now: number };

/** A source waiting this long with nothing buffered and no bytes arriving has stalled for good. */
export const STALLED_MS = 45_000;
/** Less than this many seconds ahead of the playhead is nothing to play. */
const STARVED_S = 1;

export const WATCHING: Recovery = { kind: "watching" };

/** The next state. An event that changes nothing returns `state` itself. */
export function recover(state: Recovery, event: RecoveryEvent): Recovery {
  switch (event.type) {
    case "sample": {
      const starved = event.waiting && event.ahead < STARVED_S && !event.receiving;
      switch (state.kind) {
        case "watching":
          if (event.failed) return { kind: "switching", at: event.time };
          return starved ? { kind: "suspect", since: event.now } : state;
        case "suspect":
          if (event.failed || (starved && event.now - state.since >= STALLED_MS)) return { kind: "switching", at: event.time };
          return starved ? state : WATCHING;
        case "switching":
          return state;
        case "exhausted":
          // Bytes arriving again mean this source came back by itself.
          return event.receiving ? WATCHING : state;
      }
    }
    case "playing":
      return state.kind === "switching" ? WATCHING : state;
    case "failed":
      return state.kind === "switching" ? { kind: "exhausted", error: event.error } : state;
    case "retry":
      return state.kind === "exhausted" ? { kind: "suspect", since: event.now } : state;
  }
}
