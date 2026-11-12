import { useEffect, useState } from "react";
import { thumbnailUrl } from "../api";

/** The pointer rests this long before a frame is asked for, so sweeping the bar asks for none. */
const REST_MS = 120;
const KEPT = 80;

// Frames already fetched, as object URLs, newest last. Keys are the thumbnail's own URL.
const frames = new Map<string, string>();

function keep(key: string, frame: string): void {
  frames.set(key, frame);
  const oldest = frames.keys().next().value;
  if (frames.size > KEPT && oldest !== undefined) {
    URL.revokeObjectURL(frames.get(oldest)!);
    frames.delete(oldest);
  }
}

/**
 * The picture at `time` seconds of a file, or null: while it loads, where `time` is null, and when
 * the engine has none. It is only ever a loaded image, so nothing broken can show.
 */
export function useThumbnail(id: string, file: number, time: number | null): string | null {
  const key = time === null ? null : thumbnailUrl(id, file, time);
  const [loaded, setLoaded] = useState<{ key: string; frame: string } | null>(null);

  useEffect(() => {
    if (!key || frames.has(key)) return;
    const stop = new AbortController();
    const timer = setTimeout(() => {
      fetch(key, { signal: stop.signal })
        .then((response) => (response.ok ? response.blob() : Promise.reject(new Error("no frame"))))
        .then((picture) => createImageBitmap(picture).then(() => picture))
        .then((picture) => {
          keep(key, URL.createObjectURL(picture));
          setLoaded({ key, frame: frames.get(key)! });
        })
        .catch(() => {});
    }, REST_MS);
    return () => {
      clearTimeout(timer);
      stop.abort();
    };
  }, [key]);

  if (!key) return null;
  return frames.get(key) ?? (loaded?.key === key ? loaded.frame : null);
}
