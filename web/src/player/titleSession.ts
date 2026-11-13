import { useEffect, useState } from "react";
import {
  type ApiError,
  asApiError,
  type Check,
  type EpisodeInfo,
  type MediaInfo,
  type PlayInfo,
  type PlayMessage,
  playTitle,
  type Release,
  type Skip,
  type TitleCard,
} from "../api";
import { type Fetching, readMedia } from "./ffmpeg";
import { playback } from "./plan";

/** A source the engine passed over, with its name when the engine had named it. */
export interface Passed {
  release: string;
  name: string | null;
  reason: Skip;
}

/** What the engine has said so far about one title play. */
export interface Attempt {
  /** The film or episode, which the engine chooses for a series. */
  video: string;
  title: TitleCard | null;
  episode: EpisodeInfo | null;
  /** How many sources the search found, once it has. */
  found: number | null;
  /** The source under test, and what is being checked about it. */
  trying: { rank: number; of: number; release: Release; check: Check | null } | null;
  /** Oldest first. */
  passed: Passed[];
  /** Every release the engine has named, by ID. */
  named: Record<string, Release>;
}

/** The source that plays. */
export interface Started {
  video: string;
  release: Release;
  play: PlayInfo;
  /** Seconds to start at, or null for the beginning. */
  resumeAt: number | null;
}

/** The steps between asking for a title and having something to play. */
export type TitleSession =
  | { stage: "starting"; attempt: Attempt }
  /**
   * A source plays. Waiting for its first pieces to see what is in the file, and for ffmpeg while
   * the engine fetches it to convert the file.
   */
  | { stage: "reading"; attempt: Attempt; started: Started; ffmpeg: Fetching | null }
  | { stage: "ready"; attempt: Attempt; started: Started; media: MediaInfo }
  | { stage: "failed"; attempt: Attempt; error: ApiError; tried: Passed[] };

export function beginning(video: string): Attempt {
  return { video, title: null, episode: null, found: null, trying: null, passed: [], named: {} };
}

function passed(attempt: Attempt, release: string, reason: Skip): Passed {
  return { release, name: attempt.named[release]?.name ?? null, reason };
}

/** The attempt after one line of the engine's stream. `playing` and `failed` end it, and change nothing here. */
export function heard(attempt: Attempt, message: PlayMessage): Attempt {
  switch (message.type) {
    case "resolved":
      return { ...attempt, video: message.video, title: message.title, episode: message.episode };
    case "searched":
      return { ...attempt, found: message.found };
    case "trying":
      return {
        ...attempt,
        trying: { rank: message.rank, of: message.of, release: message.release, check: null },
        named: { ...attempt.named, [message.release.id]: message.release },
      };
    case "check":
      return attempt.trying ? { ...attempt, trying: { ...attempt.trying, check: message.check } } : attempt;
    case "skipped":
      return { ...attempt, trying: null, passed: [...attempt.passed, passed(attempt, message.release, message.reason)] };
    case "playing":
    case "failed":
      return attempt;
  }
}

/** A play stream that closed before it said how the play went. */
const CUT_OFF: ApiError = {
  code: "engine_unreachable",
  message: "The engine stopped answering before a source played.",
  hint: "Make sure torrential web is still running, then try again.",
};

/**
 * Asks the engine for one source of `request.video`, reporting each step, until it plays or no source
 * does. Reads the stream to its end, since the engine treats a caller that goes away early as one that
 * no longer wants the source it is trying.
 */
export async function attempt(
  request: { video: string; source?: string; replace?: string },
  signal: AbortSignal,
  onStep: (attempt: Attempt) => void,
): Promise<{ started: Started; attempt: Attempt } | { error: ApiError; tried: Passed[]; attempt: Attempt }> {
  let now = beginning(request.video);
  let outcome: { started: Started; attempt: Attempt } | { error: ApiError; tried: Passed[]; attempt: Attempt } | null = null;
  try {
    for await (const message of playTitle({ ...request, playback: playback() }, signal)) {
      if (outcome) continue;
      if (message.type === "playing") {
        const started = { video: message.video, release: message.release, play: message.play, resumeAt: message.resume_at };
        outcome = { started, attempt: { ...now, named: { ...now.named, [message.release.id]: message.release } } };
      } else if (message.type === "failed") {
        outcome = { error: message.error, tried: message.tried.map((tried) => passed(now, tried.release, tried.reason)), attempt: now };
      } else {
        now = heard(now, message);
        onStep(now);
      }
    }
  } catch (thrown) {
    if (signal.aborted) throw thrown;
    return outcome ?? { error: asApiError(thrown), tried: now.passed, attempt: now };
  }
  return outcome ?? { error: CUT_OFF, tried: now.passed, attempt: now };
}

/** Plays a film or an episode by title and reads its video. */
export function useTitleSession(video: string, source: string | null): TitleSession {
  const [session, setSession] = useState<TitleSession>(() => ({ stage: "starting", attempt: beginning(video) }));

  useEffect(() => {
    const stop = new AbortController();
    setSession({ stage: "starting", attempt: beginning(video) });
    (async () => {
      const ended = await attempt({ video, source: source ?? undefined }, stop.signal, (now) => setSession({ stage: "starting", attempt: now }));
      if ("error" in ended) return setSession({ stage: "failed", attempt: ended.attempt, error: ended.error, tried: ended.tried });
      const { started } = ended;
      setSession({ stage: "reading", attempt: ended.attempt, started, ffmpeg: null });
      try {
        const fetching = (ffmpeg: Fetching) => setSession({ stage: "reading", attempt: ended.attempt, started, ffmpeg });
        const details = await readMedia(started.play.id, started.play.video.index, stop.signal, fetching);
        setSession({ stage: "ready", attempt: ended.attempt, started, media: details });
      } catch (thrown) {
        if (!stop.signal.aborted) setSession({ stage: "failed", attempt: ended.attempt, error: asApiError(thrown), tried: ended.attempt.passed });
      }
    })().catch(() => {});
    return () => stop.abort();
  }, [video, source]);

  return session;
}
