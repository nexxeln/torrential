import { createContext, useContext } from "react";
import type { FeedbackStore } from "./feedback";
import type { VideoStore } from "./video";

/** The time the pointer is dragging the seek bar to. Parts that draw the playhead read it each frame. */
export class Scrub {
  value: number | null = null;
  private readonly listeners = new Set<() => void>();

  set(value: number | null): void {
    const was = this.value !== null;
    this.value = value;
    // Only the start and the end of a drag are news to React.
    if (was !== (value !== null)) this.listeners.forEach((listener) => listener());
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  active = (): boolean => this.value !== null;
}

/** What every part of the player screen works with. */
export interface PlayerParts {
  element: HTMLVideoElement;
  video: VideoStore;
  feedback: FeedbackStore;
  scrub: Scrub;
}

export const PlayerContext = createContext<PlayerParts | null>(null);

export function usePlayer(): PlayerParts {
  const parts = useContext(PlayerContext);
  if (!parts) throw new Error("usePlayer needs a PlayerContext above it");
  return parts;
}
