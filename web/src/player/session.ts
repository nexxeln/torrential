import { useEffect, useState } from "react";
import { type ApiError, asApiError, type MediaInfo, play, type PlayInfo } from "../api";
import { type Fetching, readMedia } from "./ffmpeg";

/** The steps between opening a video and having something to play. */
export type Session =
  /** Waiting for the torrent's file list, which a magnet link gets from peers. */
  | { stage: "finding" }
  /**
   * The download runs. Waiting for the first pieces to see what is in the file, and for ffmpeg
   * while the engine fetches it to convert the file.
   */
  | { stage: "reading"; play: PlayInfo; ffmpeg: Fetching | null }
  | { stage: "ready"; play: PlayInfo; media: MediaInfo }
  | { stage: "failed"; error: ApiError; play: PlayInfo | null };

/** Starts the download of `target` and reads its video. A new `attempt` number tries again. */
export function useSession(target: string, file: number | null, attempt: number): Session {
  const [session, setSession] = useState<Session>({ stage: "finding" });

  useEffect(() => {
    const stop = new AbortController();
    let started: PlayInfo | null = null;
    setSession({ stage: "finding" });
    play(target, file ?? undefined, stop.signal)
      .then((info) => {
        started = info;
        setSession({ stage: "reading", play: info, ffmpeg: null });
        return readMedia(info.id, info.video.index, stop.signal, (ffmpeg) => setSession({ stage: "reading", play: info, ffmpeg }));
      })
      .then((details) => setSession({ stage: "ready", play: started!, media: details }))
      .catch((thrown) => {
        if (!stop.signal.aborted) setSession({ stage: "failed", error: asApiError(thrown), play: started });
      });
    return () => stop.abort();
  }, [target, file, attempt]);

  return session;
}
