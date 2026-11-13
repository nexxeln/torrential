import { ArrowDownIcon, ArrowLeftIcon, CheckIcon } from "@heroicons/react/16/solid";
import { type CSSProperties, type ReactNode, useEffect, useState } from "react";
import type { ApiError, Job, PlayInfo } from "../api";
import { count, percent, rate, sentence } from "../lib/format";
import { useJob } from "../lib/jobs";
import { readable } from "../lib/release";
import { isTv } from "../lib/tv";
import { Button } from "../ui/Button";
import { Spinner } from "../ui/Spinner";
import { usePlayer } from "./context";
import type { Heading } from "./Playback";
import { useVideo } from "./video";

/** How the download is doing, in a few words. Nothing once it is done. */
export function Readout({ job }: { job: Job }) {
  const { progress } = job;
  switch (job.state) {
    case "downloading":
      return (
        <div className="flex items-center gap-1 tabular-nums">
          <ArrowDownIcon className="size-4 shrink-0 fill-action" />
          {rate(progress.down_rate)} · {count(progress.peers, "peer")}
          {progress.size ? ` · ${percent(progress.verified / progress.size)}` : ""}
        </div>
      );
    case "metadata":
      return <div>Finding peers</div>;
    case "queued":
      return <div>Waiting for a download slot</div>;
    case "checking":
      return <div>Checking files on disk</div>;
    case "paused":
      return <div>Download paused</div>;
    case "error":
      return <div className="text-error">{sentence(job.error?.message ?? "The download failed")}</div>;
    case "done":
      return null;
  }
}

/** The download behind the video, as it is now. Only what calls this renders when the download moves. */
export function usePlayingJob(play: PlayInfo): Job {
  return useJob(play.id) ?? play.job;
}

/** The readout of the playing video's own download. */
export function PlayingReadout({ play }: { play: PlayInfo }) {
  return <Readout job={usePlayingJob(play)} />;
}

interface TopBarProps {
  /** The release name, once known. */
  name: string | null;
  /** A title's own name, which takes the place of the release name. */
  heading?: Heading | null;
  /** The file's name, when the torrent has several videos. */
  file: string | null;
  /** How the download is doing, at the right. */
  readout?: ReactNode;
  onBack: () => void;
}

/** The title at the top left, over a scrim, with the way back and the download's state. */
export function TopBar({ name, heading, file, readout, onBack }: TopBarProps) {
  const release = name ? readable(name) : null;
  const own = file ? readable(file) : null;
  // A file of a season reads as the show over the episode. A single film reads as its title over its tags.
  const whole = own ? `${own.title} ${own.tags}`.trim() : null;
  const episode = whole && release && whole.startsWith(release.title) ? whole.slice(release.title.length).trim() : whole;
  const title = heading?.title ?? release?.title;
  const under = heading ? heading.detail : episode || release?.tags;
  return (
    <header className="flex items-start gap-2 bg-linear-to-b from-black/80 via-black/40 to-transparent pt-(--edge-top) pr-(--edge-right) pb-16 pl-(--edge-left)">
      <button
        type="button"
        aria-label="Back (Esc)"
        title="Back (Esc)"
        onClick={onBack}
        className="relative flex size-10 shrink-0 items-center justify-center rounded-full hover:bg-white/15 pointer-coarse:size-11"
      >
        <ArrowLeftIcon className="size-4 fill-ink" />
      </button>
      <div className="flex min-w-0 flex-1 flex-col py-0.5 text-over-video">
        {title && <h1 className="truncate text-base font-medium text-ink">{title}</h1>}
        {under && <p className="truncate text-sm text-ink/60">{under}</p>}
      </div>
      {readout && <div className="shrink-0 py-2.5 text-sm text-ink/75 text-over-video max-sm:hidden">{readout}</div>}
    </header>
  );
}

const STEPS = [
  { title: "Finding the files", detail: "Asking peers what is in this torrent. With few peers this can take up to a minute." },
  { title: "Reading the video", detail: "Downloading its first pieces to see what is inside." },
  { title: "Getting ready to play", detail: "Buffering the first seconds." },
];

interface StartingProps {
  /** The step in progress: 0 finding the files, 1 reading the video, 2 buffering. */
  step: 0 | 1 | 2;
  name: string | null;
  /** How the download is doing, under the steps. */
  readout: ReactNode;
}

/** The screen before the first frame: what is being played, and which step toward playing is running. */
export function Starting({ step, name, readout }: StartingProps) {
  const release = name ? readable(name) : null;
  return (
    <div className="absolute inset-0 flex items-end bg-radial-[at_15%_95%] from-raised to-black to-70% pr-[max(--spacing(6),7vw,env(safe-area-inset-right))] pb-[max(--spacing(8),11vh)] pl-[max(--spacing(6),7vw,env(safe-area-inset-left))]">
      <div className="flex max-w-xl animate-appear flex-col gap-8 motion-reduce:animate-none">
        {release && (
          <div className="flex flex-col gap-2">
            <h1 className="text-3xl font-semibold tracking-tight text-balance text-ink sm:text-4xl">{release.title}</h1>
            {release.tags && <p className="text-sm text-pencil">{release.tags}</p>}
          </div>
        )}
        <ol role="list" aria-label="Starting" className="flex flex-col gap-3">
          {STEPS.map((stage, at) => (
            <li
              key={stage.title}
              aria-current={at === step ? "step" : undefined}
              data-state={at < step ? "done" : at === step ? "now" : "next"}
              className="group/step flex items-start gap-3 text-base text-pencil data-[state=done]:text-soft data-[state=now]:text-ink"
            >
              <div className="flex h-lh w-5 shrink-0 items-center justify-center">
                {at < step && <CheckIcon className="size-4 fill-action" />}
                {at === step && <Spinner className="size-4 text-ink" />}
                {at > step && <div className="size-1.5 rounded-full bg-rule" />}
              </div>
              <div className="flex flex-col gap-0.5">
                <p className="group-data-[state=now]/step:font-medium">{stage.title}</p>
                {at === step && <p className="max-w-[46ch] text-sm text-pretty text-pencil">{stage.detail}</p>}
              </div>
            </li>
          ))}
        </ol>
        <div className="min-h-5 text-sm text-soft">{readout}</div>
      </div>
    </div>
  );
}

/** The spinner waits this long, so short seeks and quick refills do not flash it. */
const PATIENCE_MS = 450;
/** With no bytes arriving for this long, the wait is on the swarm. */
const STALLED_MS = 8000;

/**
 * Shown when playback has to wait in the middle of a video. Says what it is waiting for: pieces
 * arriving at some rate from some peers, or a swarm that is sending nothing.
 */
export function Buffering({ play, quiet = false }: { play: PlayInfo; quiet?: boolean }) {
  const job = usePlayingJob(play);
  const waiting = useVideo(usePlayer().video, (state) => state.started && state.waiting && !state.ended);
  const [patient, setPatient] = useState(true);
  const [stalled, setStalled] = useState(false);
  const flowing = job.progress.down_rate > 0;
  const fetching = job.state !== "done";

  useEffect(() => {
    setPatient(true);
    if (!waiting) return;
    const timer = setTimeout(() => setPatient(false), PATIENCE_MS);
    return () => clearTimeout(timer);
  }, [waiting]);
  useEffect(() => {
    setStalled(false);
    if (!waiting || flowing || !fetching) return;
    const timer = setTimeout(() => setStalled(true), STALLED_MS);
    return () => clearTimeout(timer);
  }, [waiting, flowing, fetching]);

  if (!waiting || patient) return null;
  const { progress } = job;
  return (
    <div role="status" className="pointer-events-none absolute inset-0 flex items-center justify-center p-6 text-center">
      <div className="flex flex-col items-center gap-3 rounded-2xl bg-black/55 px-6 py-5 backdrop-blur-md">
        <Spinner className="size-8 text-white" />
        {fetching && !quiet && (
          <p className="max-w-[40ch] text-sm text-pretty text-white/90 tabular-nums">
            {job.state !== "downloading"
              ? "This part of the video has not downloaded, and the download is not running."
              : stalled
                ? "No peer is sending this part right now. Playback continues as soon as one does."
                : `Waiting for pieces · ${rate(progress.down_rate)} from ${count(progress.peers, "peer")}`}
          </p>
        )}
      </div>
    </div>
  );
}

const UP_NEXT_SECONDS = 10;

interface EndedProps {
  /** The next video of the torrent, as a title, when there is one. */
  next: string | null;
  onNext: () => void;
  onReplay: () => void;
  onBack: () => void;
  /** What the way back says. */
  back?: string;
}

/** The end of a video. A season goes on to its next episode after a countdown that can be cancelled. */
export function Ended({ next, onNext, onReplay, onBack, back = "Back to search" }: EndedProps) {
  const [left, setLeft] = useState(next ? UP_NEXT_SECONDS : null);
  useEffect(() => {
    if (left === null) return;
    if (left === 0) return onNext();
    const timer = setTimeout(() => setLeft(left - 1), 1000);
    return () => clearTimeout(timer);
  }, [left, onNext]);

  return (
    <div className="absolute inset-0 flex flex-col items-center justify-center gap-6 bg-black/70 p-6 text-center">
      {next ? (
        <div className="flex max-w-[52ch] flex-col gap-1">
          <p className="text-sm text-soft tabular-nums" aria-live="polite">
            {left === null ? "Up next" : `Up next in ${left}`}
          </p>
          <h2 className="text-2xl font-semibold tracking-tight text-balance text-ink">{next}</h2>
        </div>
      ) : (
        <h2 className="text-lg font-medium text-ink">The end</h2>
      )}
      {left !== null && (
        <div className="h-0.5 w-48 overflow-hidden rounded-full bg-white/15">
          <div
            className="h-full origin-left animate-countdown bg-ink motion-reduce:animate-none"
            style={{ animationDuration: `${UP_NEXT_SECONDS}s` } as CSSProperties}
          />
        </div>
      )}
      <div className="flex flex-wrap justify-center gap-2">
        {next && (
          <Button variant="primary" autoFocus={isTv()} onClick={onNext}>
            Play now
          </Button>
        )}
        {left !== null && <Button onClick={() => setLeft(null)}>Cancel</Button>}
        <Button variant={next ? "secondary" : "primary"} autoFocus={isTv() && !next} onClick={onReplay}>
          Watch again
        </Button>
        {left === null && <Button onClick={onBack}>{back}</Button>}
      </div>
    </div>
  );
}

interface ProblemProps {
  title: string;
  error?: ApiError;
  children?: ReactNode;
  /** The command that fixes it, shown to copy. */
  fix?: string;
  /** Another way out, under the fix. */
  note?: ReactNode;
  onRetry: () => void;
  onBack: () => void;
  /** What the way back says. */
  back?: string;
}

/** Playback cannot go on. Says why in the engine's words, and what to do. */
export function Problem({ title, error, children, fix, note, onRetry, onBack, back = "Back to search" }: ProblemProps) {
  return (
    <div role="alert" className="absolute inset-0 flex flex-col items-center justify-center gap-5 bg-black/70 p-6 text-center">
      <div className="flex max-w-[52ch] flex-col gap-2">
        <h2 className="text-lg font-medium text-balance text-ink">{title}</h2>
        {error && <p className="text-sm text-pretty text-soft">{sentence(error.message)}</p>}
        {error?.hint && <p className="text-sm text-pretty text-pencil">{sentence(error.hint)}</p>}
        {children}
      </div>
      {fix && (
        <code className="rounded-lg bg-white/5 px-3 py-2 font-mono text-sm text-ink inset-ring inset-ring-white/10 select-all">
          {fix}
        </code>
      )}
      {note && <p className="max-w-[52ch] text-sm text-pretty text-pencil">{note}</p>}
      <div className="flex gap-2">
        <Button variant="primary" autoFocus={isTv()} onClick={onRetry}>
          Try again
        </Button>
        <Button onClick={onBack}>{back}</Button>
      </div>
    </div>
  );
}
