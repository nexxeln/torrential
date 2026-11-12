import { useSyncExternalStore } from "react";

/**
 * What the player's chrome shows of a video element, without the playhead: time and buffered
 * ranges change many times a second, and the parts that draw them read the element themselves.
 */
export interface VideoState {
  paused: boolean;
  /** Not enough data to play at the current time. */
  waiting: boolean;
  /** A frame has been shown since the element was given a file. */
  started: boolean;
  ended: boolean;
  volume: number;
  muted: boolean;
  speed: number;
  /** Seconds, or null until the element knows. */
  duration: number | null;
  pictureInPicture: boolean;
}

// `timeupdate` is here to catch what no other event announces, such as data arriving for a stalled
// playhead. It changes nothing most of the time, and then nothing renders.
const EVENTS = [
  "loadstart", "emptied", "loadedmetadata", "loadeddata", "durationchange", "canplay", "canplaythrough", "playing",
  "waiting", "stalled", "play", "pause", "seeking", "seeked", "timeupdate", "ended", "volumechange", "ratechange",
  "enterpictureinpicture", "leavepictureinpicture",
] as const;

const same = (a: VideoState, b: VideoState) => (Object.keys(a) as (keyof VideoState)[]).every((key) => a[key] === b[key]);

/** Follows one video element while anything subscribes. */
export class VideoStore {
  private state: VideoState;
  private started = false;
  private readonly listeners = new Set<() => void>();
  private stop: AbortController | null = null;

  constructor(readonly element: HTMLVideoElement) {
    this.state = this.read();
  }

  private read(): VideoState {
    const { element } = this;
    return {
      paused: element.paused,
      waiting: !element.ended && (element.seeking || element.readyState < HTMLMediaElement.HAVE_FUTURE_DATA),
      started: this.started,
      ended: element.ended,
      volume: element.volume,
      muted: element.muted,
      speed: element.playbackRate,
      duration: Number.isFinite(element.duration) ? element.duration : null,
      pictureInPicture: document.pictureInPictureElement === element,
    };
  }

  private update = (event?: Event): void => {
    if (event?.type === "emptied") this.started = false;
    const { element } = this;
    const moving = !element.paused && element.currentTime > 0 && element.readyState >= HTMLMediaElement.HAVE_FUTURE_DATA;
    if (event?.type === "playing" || (event?.type === "canplay" && element.currentTime > 0) || moving) this.started = true;
    const next = this.read();
    if (same(next, this.state)) return;
    this.state = next;
    this.listeners.forEach((listener) => listener());
  };

  subscribe = (listener: () => void): (() => void) => {
    if (this.listeners.size === 0) {
      this.stop = new AbortController();
      for (const name of EVENTS) this.element.addEventListener(name, this.update, { signal: this.stop.signal });
      this.update();
    }
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0) this.stop?.abort();
    };
  };

  snapshot = (): VideoState => this.state;
}

/** One value of the video's state. The component renders again only when that value changes. */
export function useVideo<T>(store: VideoStore, select: (state: VideoState) => T): T {
  return useSyncExternalStore(store.subscribe, () => select(store.snapshot()));
}

/** The buffered range the playhead is in, or the first one that starts within `reach` seconds after it. */
export function bufferedAround(element: HTMLVideoElement, reach = 0.5): [number, number] | null {
  const { buffered, currentTime } = element;
  for (let i = 0; i < buffered.length; i++) {
    if (currentTime >= buffered.start(i) - reach && currentTime < buffered.end(i)) return [buffered.start(i), buffered.end(i)];
  }
  return null;
}
