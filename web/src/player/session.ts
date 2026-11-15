import { type DependencyList, useEffect, useState } from "react";
import { type ApiError, asApiError, type MediaInfo, play, type PlayInfo } from "../api";
import { type Fetching, readMedia } from "./ffmpeg";

/**
 * The steps between opening a video and having something to play, whichever way it starts. `P` is what the start has
 * said so far, and `S` what started, with the download that plays it.
 */
export type Session<P, S extends { play: PlayInfo }> =
  | { stage: "starting"; progress: P }
  /**
   * The download runs. Waiting for the first pieces to see what is in the file, and for ffmpeg while the engine fetches
   * it to convert the file.
   */
  | { stage: "reading"; progress: P; started: S; ffmpeg: Fetching | null }
  | { stage: "ready"; progress: P; started: S; media: MediaInfo }
  | { stage: "failed"; progress: P; error: ApiError; started: S | null };

/** How a start ended: what started, or why nothing did. */
export type Begun<P, S> = { started: S; progress: P } | { error: ApiError; progress: P };

/**
 * Starts a video with `begin`, which says its progress as it goes, then reads the video that started. Starts again when
 * `deps` change.
 */
export function useVideoSession<P, S extends { play: PlayInfo }>(
  first: P,
  begin: (signal: AbortSignal, report: (progress: P) => void) => Promise<Begun<P, S>>,
  deps: DependencyList,
): Session<P, S> {
  const [session, setSession] = useState<Session<P, S>>({ stage: "starting", progress: first });

  useEffect(() => {
    const stop = new AbortController();
    let progress = first;
    let started: S | null = null;
    setSession({ stage: "starting", progress });
    (async () => {
      const begun = await begin(stop.signal, (now) => {
        progress = now;
        setSession({ stage: "starting", progress: now });
      });
      progress = begun.progress;
      if ("error" in begun) return setSession({ stage: "failed", progress, error: begun.error, started: null });
      const ready = begun.started;
      started = ready;
      setSession({ stage: "reading", progress, started: ready, ffmpeg: null });
      const fetching = (ffmpeg: Fetching) => setSession({ stage: "reading", progress, started: ready, ffmpeg });
      const media = await readMedia(ready.play.id, ready.play.video.index, stop.signal, fetching);
      setSession({ stage: "ready", progress, started: ready, media });
    })().catch((thrown) => {
      if (!stop.signal.aborted) setSession({ stage: "failed", progress, error: asApiError(thrown), started });
    });
    return () => stop.abort();
    // `deps` stand for what `first` and `begin` read.
  }, deps);

  return session;
}

/** Starts the download of `target` and reads its video. A new `attempt` number tries again. */
export function useSession(target: string, file: number | null, attempt: number): Session<null, { play: PlayInfo }> {
  return useVideoSession<null, { play: PlayInfo }>(
    null,
    async (signal) => ({ started: { play: await play(target, file ?? undefined, signal) }, progress: null }),
    [target, file, attempt],
  );
}
