import { type Dispatch, type RefObject, useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import { artUrl, asApiError, type MediaInfo, media, type PlayInfo, saveWatched, type TitlePage } from "../api";
import { useJob } from "../lib/jobs";
import { goBack, navigate, playPath, titlePath } from "../router";
import { useTitle } from "../titles/useTitle";
import { episodeCode, episodeName, findEpisode, playingOrder, titleOf, videoOf } from "../titles/words";
import { Button } from "../ui/Button";
import { Spinner } from "../ui/Spinner";
import { PlayingReadout, Readout, TopBar } from "./Chrome";
import { usePlayer } from "./context";
import { command } from "./keys";
import { type Heading, type Memory, type Neighbor, Playback, type Titled } from "./Playback";
import { type Recovery, type RecoveryEvent, recover, WATCHING } from "./recovery";
import { type Naming, TitleFailed, TitleStarting } from "./TitleScreens";
import { attempt, type Started, useTitleSession } from "./titleSession";
import { bufferedAround } from "./video";

/** How long the player says it moved to another source. */
const SWITCHED_MS = 6000;

/** The source on screen: the first that played, or one that took over from a source that stalled. */
interface Current {
  started: Started;
  media: MediaInfo;
  /** Seconds. */
  start: number;
  resumed: boolean;
  poster: string | null;
}

/** The frame on screen, to keep showing while the next source loads. */
function still(element: HTMLVideoElement | null): string | null {
  if (!element || element.videoWidth === 0) return null;
  const canvas = document.createElement("canvas");
  canvas.width = element.videoWidth;
  canvas.height = element.videoHeight;
  try {
    canvas.getContext("2d")?.drawImage(element, 0, 0);
    return canvas.toDataURL("image/jpeg", 0.85);
  } catch {
    return null;
  }
}

/** The aired episodes on either side of `video`, in playing order. */
function neighbors(page: TitlePage | null, video: string): { previous: Neighbor | null; next: Neighbor | null } {
  const here = page && findEpisode(page, video);
  if (!page || !here) return { previous: null, next: null };
  const order = playingOrder(page, here.season === 0);
  const at = order.findIndex((episode) => episode.season === here.season && episode.episode === here.episode);
  const neighbor = (at: number): Neighbor | null => {
    const episode = order[at];
    if (!episode) return null;
    return {
      name: `${episodeCode(episode)} · ${episodeName(episode)}`,
      go: () => navigate(playPath(videoOf(page.id, episode)), { replace: true }),
    };
  };
  return at === -1 ? { previous: null, next: null } : { previous: neighbor(at - 1), next: neighbor(at + 1) };
}

/** Tells the recovery what the player sees, once a second. */
function StallWatch({ play, element, dispatch }: { play: PlayInfo; element: RefObject<HTMLVideoElement | null>; dispatch: Dispatch<RecoveryEvent> }) {
  const parts = usePlayer();
  const job = useRef(play.job);
  job.current = useJob(play.id) ?? play.job;

  useEffect(() => {
    element.current = parts.element;
    const timer = setInterval(() => {
      const { progress, state } = job.current;
      const playing = parts.element;
      const range = bufferedAround(playing);
      dispatch({
        type: "sample",
        now: performance.now(),
        time: playing.currentTime,
        waiting: parts.video.snapshot().waiting && !playing.paused,
        ahead: range ? range[1] - playing.currentTime : 0,
        receiving: progress.down_rate > 0 || state === "done",
        failed: state === "error",
      });
    }, 1000);
    return () => clearInterval(timer);
  }, [parts, element, dispatch]);
  return null;
}

/** What recovery is doing, over the picture. */
function RecoveryNotice({ recovery, switched, onRetry }: { recovery: Recovery; switched: boolean; onRetry: () => void }) {
  let notice = null;
  if (recovery.kind === "switching") {
    notice = (
      <>
        <Spinner className="size-4 text-ink" />
        <p className="py-1.5 pr-2">This source stopped sending video. Trying another one.</p>
      </>
    );
  } else if (recovery.kind === "exhausted") {
    notice = (
      <>
        <p className="py-1.5">No other source plays right now. Still waiting on this one.</p>
        <Button variant="quiet" className="px-2 py-1" onClick={onRetry}>
          Look again
        </Button>
      </>
    );
  } else if (switched) {
    notice = <p className="py-1.5 pr-2">Switched to another source.</p>;
  }
  return (
    <div role="status" className="pointer-events-none absolute inset-x-0 top-[max(--spacing(18),10vh)] z-20 flex justify-center px-4">
      {notice && (
        <div className="pointer-events-auto flex max-w-full animate-appear items-center gap-3 rounded-xl bg-black/70 py-1 pr-1 pl-4 text-sm text-pretty text-ink backdrop-blur-md motion-reduce:animate-none">
          {notice}
        </div>
      )}
    </div>
  );
}

interface Props {
  /** A film, an episode, or a series for its next episode. */
  video: string;
  /** A release to play instead of letting the engine choose. */
  source: string | null;
}

/** The player page for a film or an episode: the engine finds a source that plays, then it plays. */
export function TitlePlayer({ video, source }: Props) {
  const [round, setRound] = useState(0);
  const again = useCallback(() => setRound((n) => n + 1), []);
  return <Session key={round} video={video} source={source} onRetry={again} />;
}

function Session({ video, source, onRetry }: Props & { onRetry: () => void }) {
  const id = titleOf(video);
  const session = useTitleSession(video, source);
  const [info] = useTitle(id);
  const page = info.kind === "ready" ? info.page : null;
  const back = useCallback(() => goBack(titlePath(id)), [id]);
  const backdrop = page && !page.art.background ? null : artUrl(id, "background");

  const { attempt: heard } = session;
  const playing = session.stage === "reading" || session.stage === "ready" ? session.started.video : heard.video;
  const episode = heard.episode ?? (page && findEpisode(page, playing));
  const name = heard.title?.name ?? page?.name ?? null;
  const year = heard.title?.year ?? page?.year ?? null;
  const naming = useMemo<Naming>(
    () => ({ title: name, detail: episode ? `${episodeCode(episode)} · ${episodeName(episode)}` : year ? String(year) : null }),
    [name, episode, year],
  );
  const kind = playing.includes(":") || page?.kind === "series" ? "episode" : "film";

  useEffect(() => {
    document.title = name ? `${name}${episode ? ` ${episodeCode(episode)}` : ""} · torrential` : "torrential";
    return () => void (document.title = "torrential");
  }, [name, episode]);

  const [recovery, dispatch] = useReducer(recover, WATCHING);
  const [swapped, setSwapped] = useState<Current | null>(null);
  const [switched, setSwitched] = useState(false);
  const element = useRef<HTMLVideoElement | null>(null);
  const first = useMemo<Current | null>(
    () =>
      session.stage === "ready"
        ? { started: session.started, media: session.media, start: session.started.resumeAt ?? 0, resumed: (session.started.resumeAt ?? 0) > 0, poster: null }
        : null,
    [session],
  );
  const current = swapped ?? first;
  const live = useRef(current);
  live.current = current;

  const switching = recovery.kind === "switching";
  useEffect(() => {
    const from = live.current;
    if (!switching || !from) return;
    const stop = new AbortController();
    (async () => {
      const ended = await attempt({ video: from.started.video, replace: from.started.play.id }, stop.signal, () => {});
      if ("error" in ended) return dispatch({ type: "failed", error: ended.error });
      const details = await media(ended.started.play.id, ended.started.play.video.index, stop.signal);
      const at = element.current?.currentTime ?? from.start;
      setSwapped({ started: ended.started, media: details, start: at, resumed: false, poster: still(element.current) });
      setSwitched(true);
      dispatch({ type: "playing" });
    })().catch((thrown) => {
      if (!stop.signal.aborted) dispatch({ type: "failed", error: asApiError(thrown) });
    });
    return () => stop.abort();
  }, [switching]);
  useEffect(() => {
    if (!switched) return;
    const timer = setTimeout(() => setSwitched(false), SWITCHED_MS);
    return () => clearTimeout(timer);
  }, [switched]);

  const titled = useMemo<Titled | null>(() => {
    if (!current) return null;
    const { started } = current;
    const heading: Heading = { title: naming.title ?? "", detail: naming.detail };
    const duration = current.media.duration;
    const memory: Memory = {
      start: current.start,
      resumed: current.resumed,
      save: (time, length) => void saveWatched({ video: started.video, position: time, duration: length, release: started.release.id }).catch(() => {}),
      forget: () => void saveWatched({ video: started.video, position: 0, duration, release: started.release.id }).catch(() => {}),
    };
    return {
      kind,
      heading,
      ...neighbors(page, started.video),
      memory,
      cover: current.poster ? null : (
        <TitleStarting attempt={heard} naming={naming} backdrop={backdrop} step={3} readout={<PlayingReadout play={started.play} />} />
      ),
      poster: current.poster,
      overlay: <StallWatch play={started.play} element={element} dispatch={dispatch} />,
      replacing: switching,
    };
  }, [current, naming, kind, page, heard, backdrop, switching]);

  const ready = titled !== null;
  useEffect(() => {
    if (ready) return;
    const pressed = (event: KeyboardEvent) => {
      if (command(event)?.kind === "leave") back();
    };
    addEventListener("keydown", pressed);
    return () => removeEventListener("keydown", pressed);
  }, [ready, back]);

  const reading = session.stage === "reading" ? session.started.play : null;
  const readingJob = useJob(reading?.id ?? null) ?? reading?.job ?? null;

  return (
    <main className="fixed inset-0 bg-black text-soft">
      {current && titled ? (
        <>
          <Playback key={current.started.play.id} play={current.started.play} media={current.media} onReload={onRetry} onBack={back} titled={titled} />
          <RecoveryNotice recovery={recovery} switched={switched} onRetry={() => dispatch({ type: "retry", now: performance.now() })} />
        </>
      ) : (
        <div className="relative size-full">
          {session.stage === "failed" ? (
            <TitleFailed
              naming={naming}
              backdrop={backdrop}
              kind={kind}
              error={session.error}
              tried={session.tried}
              sources={titlePath(id, { sources: heard.video })}
              unpinned={source ? playPath(video) : null}
              onRetry={onRetry}
              onBack={back}
            />
          ) : (
            <TitleStarting
              attempt={heard}
              naming={naming}
              backdrop={backdrop}
              step={session.stage === "reading" ? 2 : heard.found === null ? 0 : 1}
              readout={readingJob && <Readout job={readingJob} />}
            />
          )}
          <div className="absolute inset-x-0 top-0 z-10">
            <TopBar name={null} file={null} onBack={back} />
          </div>
        </div>
      )}
    </main>
  );
}
