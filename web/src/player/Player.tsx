import { useCallback, useMemo, useState } from "react";
import type { PlayInfo } from "../api";
import { useJob } from "../lib/jobs";
import { fileName, isVideoFile, readable } from "../lib/release";
import { forgetPosition, position, savePosition } from "../lib/storage";
import { goBack, navigate, watchPath } from "../router";
import { WORTH_SAVING } from "../wire";
import { FfmpegReadout, PlayingReadout, Problem, Readout, releaseHeading, Starting, TopBar } from "./Chrome";
import { usePlayerPage } from "./page";
import { type Memory, type Neighbor, Playback, type PlaybackProps, type Shown } from "./Playback";
import { useSession } from "./session";

const INFOHASH = /^[0-9a-f]{40}$/;

interface Props {
  /** A release or download ID, magnet link, or infohash. */
  target: string;
  /** The file to play, or null for the engine's pick. */
  file: number | null;
}

/** The player page: starts the download, says what it is waiting for, then plays. */
export function Player({ target, file }: Props) {
  const [attempt, setAttempt] = useState(0);
  const session = useSession(target, file, attempt);
  const play = session.stage === "starting" ? null : (session.started?.play ?? null);
  const live = useJob(play?.id ?? (INFOHASH.test(target) ? target : null));
  const job = live ?? play?.job ?? null;
  const back = useCallback(() => goBack("/"), []);
  const retry = useCallback(() => setAttempt((n) => n + 1), []);

  const name = play?.name ?? job?.name ?? null;
  usePlayerPage(name && readable(name).title, session.stage === "ready", back);

  return (
    <main className="fixed inset-0 bg-black text-soft">
      {session.stage === "ready" ? (
        <TorrentPlayback key={`${session.started.play.id}:${session.started.play.video.index}`} play={session.started.play} media={session.media} onReload={retry} onBack={back} />
      ) : (
        <div className="relative size-full">
          {session.stage === "failed" ? (
            <Problem title="This video cannot start" error={session.error} onRetry={retry} onBack={back} />
          ) : (
            <Starting
              step={session.stage === "starting" ? 0 : session.ffmpeg ? 2 : 1}
              name={name}
              readout={session.stage === "reading" && session.ffmpeg ? <FfmpegReadout fetching={session.ffmpeg} /> : job && <Readout job={job} />}
            />
          )}
          <div className="absolute inset-x-0 top-0 z-10">
            <TopBar heading={session.stage === "failed" && name ? releaseHeading(name, null) : null} onBack={back} />
          </div>
        </div>
      )}
    </main>
  );
}

/** A file of a torrent in the player: named by its release, moving through the torrent's videos, and keeping its place in this browser. */
function TorrentPlayback(props: Omit<PlaybackProps, "shown">) {
  const { play } = props;
  const shown = useMemo(() => torrentShown(play), [play]);
  return <Playback {...props} shown={shown} />;
}

function torrentShown(play: PlayInfo): Shown {
  const { id, name, video: file } = play;
  const videos = play.files.filter((f) => isVideoFile(f.path)).sort((a, b) => a.path.localeCompare(b.path, "en", { numeric: true }));
  const here = videos.findIndex((f) => f.index === file.index);
  const neighbor = (at: number): Neighbor | null => {
    const other = videos[at];
    return other ? { name: episodeName(other.path, name), go: () => navigate(watchPath(id, other.index), { replace: true }) } : null;
  };
  const multiple = videos.length > 1;
  const release = readable(name).title;
  return {
    kind: "torrent",
    video: null,
    heading: releaseHeading(name, multiple ? fileName(file.path) : null),
    nowPlaying: { title: multiple ? episodeName(file.path, name) : release, album: release },
    previous: neighbor(here - 1),
    next: neighbor(here + 1),
    videos,
    memory: torrentMemory(play),
    cover: <Starting step={2} name={name} readout={<PlayingReadout play={play} />} />,
    poster: null,
    overlay: null,
    replacing: false,
  };
}

/** A file of a season by its own name: the episode, without the show's title that its release already says. */
function episodeName(path: string, release: string): string {
  const own = readable(fileName(path));
  const whole = `${own.title} ${own.tags}`.trim();
  const show = readable(release).title;
  return (whole.startsWith(show) && whole.slice(show.length).trim()) || whole;
}

/** A torrent's file starts where this browser saw it stop, and keeps its place there. */
function torrentMemory(play: PlayInfo): Memory {
  const { id, name, video: file } = play;
  const stored = position(id, file.index);
  const start = stored && !stored.finished && stored.time > WORTH_SAVING ? stored.time : 0;
  return {
    start,
    resumed: start > 0,
    save: (time, duration) => {
      if (duration && time > WORTH_SAVING) savePosition({ id, file: file.index, name, path: file.path, time, duration });
    },
    forget: () => forgetPosition(id, file.index),
  };
}
