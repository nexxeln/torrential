import { useEffect, useState } from "react";
import type { PlayerActions } from "./actions";
import type { VideoStore } from "./video";
import { useVideo } from "./video";

interface NowPlaying {
  title: string;
  /** The release the video belongs to. */
  album: string;
  actions: PlayerActions;
  video: VideoStore;
  /** Plays the next or previous video of the torrent, where there is one. */
  onNext: (() => void) | null;
  onPrevious: (() => void) | null;
}

/** Tells the system what is playing, and answers its media keys and now-playing controls. */
export function useMediaSession({ title, album, actions, video, onNext, onPrevious }: NowPlaying): void {
  const paused = useVideo(video, (state) => state.paused);
  useEffect(() => {
    if (!("mediaSession" in navigator)) return;
    const session = navigator.mediaSession;
    session.metadata = new MediaMetadata({ title, album });
    const handlers: [MediaSessionAction, MediaSessionActionHandler | null][] = [
      ["play", () => actions.play()],
      ["pause", () => video.element.pause()],
      ["seekbackward", (details) => actions.seekBy(-(details.seekOffset ?? 10))],
      ["seekforward", (details) => actions.seekBy(details.seekOffset ?? 10)],
      ["seekto", (details) => details.seekTime !== undefined && actions.seekTo(details.seekTime)],
      ["previoustrack", onPrevious],
      ["nexttrack", onNext],
    ];
    for (const [action, handler] of handlers) session.setActionHandler(action, handler);
    return () => {
      session.metadata = null;
      for (const [action] of handlers) session.setActionHandler(action, null);
    };
  }, [title, album, actions, video, onNext, onPrevious]);

  useEffect(() => {
    if ("mediaSession" in navigator) navigator.mediaSession.playbackState = paused ? "paused" : "playing";
  }, [paused]);
}

/** Keeps the screen awake while the video plays. The lock is given back on pause and when the tab hides. */
export function useWakeLock(video: VideoStore): void {
  const playing = useVideo(video, (state) => !state.paused && !state.ended);
  useEffect(() => {
    if (!playing || !("wakeLock" in navigator)) return;
    let lock: WakeLockSentinel | null = null;
    let released = false;
    const take = () => {
      if (document.visibilityState !== "visible") return;
      navigator.wakeLock
        .request("screen")
        .then((taken) => {
          lock = taken;
          if (released) void taken.release();
        })
        .catch(() => {});
    };
    take();
    document.addEventListener("visibilitychange", take);
    return () => {
      released = true;
      document.removeEventListener("visibilitychange", take);
      void lock?.release();
    };
  }, [playing]);
}

/** Safari's own names for AirPlay. */
interface AirPlayElement extends HTMLVideoElement {
  webkitShowPlaybackTargetPicker?: () => void;
}

/**
 * Opens Safari's AirPlay picker, or null where there is no AirPlay screen to send to. Only a
 * file the browser plays itself can go to one, which `enabled` says.
 */
export function useAirPlay(element: HTMLVideoElement, enabled: boolean): (() => void) | null {
  const [available, setAvailable] = useState(false);
  useEffect(() => {
    const changed = (event: Event) => setAvailable((event as Event & { availability?: string }).availability === "available");
    element.addEventListener("webkitplaybacktargetavailabilitychanged", changed);
    return () => element.removeEventListener("webkitplaybacktargetavailabilitychanged", changed);
  }, [element]);
  const picker = (element as AirPlayElement).webkitShowPlaybackTargetPicker;
  return enabled && available && picker ? () => picker.call(element) : null;
}
