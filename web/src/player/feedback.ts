import { useSyncExternalStore } from "react";

/** What an action just did, shown over the video for a moment. */
export interface Feedback {
  /** A symbol in the middle of the picture. */
  bezel: { icon: "play" | "pause"; at: number } | null;
  /** Seconds jumped, summed while jumps keep coming, on the side they went to. */
  seek: { seconds: number; at: number } | null;
  /** A line at the top: what changed and its new value. `level` draws a bar from 0 to 1. */
  note: { text: string; level?: number; at: number } | null;
}

const BEZEL_MS = 500;
const SEEK_MS = 900;
const NOTE_MS = 1400;

export class FeedbackStore {
  private state: Feedback = { bezel: null, seek: null, note: null };
  private readonly listeners = new Set<() => void>();
  private readonly timers: Partial<Record<keyof Feedback, ReturnType<typeof setTimeout>>> = {};

  private set<K extends keyof Feedback>(slot: K, value: Feedback[K], ms: number): void {
    this.state = { ...this.state, [slot]: value };
    clearTimeout(this.timers[slot]);
    if (value) this.timers[slot] = setTimeout(() => this.set(slot, null, 0), ms);
    this.listeners.forEach((listener) => listener());
  }

  bezel(icon: "play" | "pause"): void {
    this.set("bezel", { icon, at: performance.now() }, BEZEL_MS);
  }

  /** Jumps in one direction add up while the indicator is still showing. */
  seek(seconds: number): void {
    const shown = this.state.seek;
    const sum = shown && Math.sign(shown.seconds) === Math.sign(seconds) ? shown.seconds + seconds : seconds;
    this.set("seek", { seconds: sum, at: performance.now() }, SEEK_MS);
  }

  note(text: string, level?: number): void {
    this.set("note", { text, level, at: performance.now() }, NOTE_MS);
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  snapshot = (): Feedback => this.state;
}

export function useFeedback(store: FeedbackStore): Feedback {
  return useSyncExternalStore(store.subscribe, store.snapshot);
}
