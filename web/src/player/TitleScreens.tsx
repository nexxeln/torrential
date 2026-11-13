import { CheckIcon } from "@heroicons/react/16/solid";
import type { ReactNode } from "react";
import type { ApiError, Release } from "../api";
import { bytes, count, sentence } from "../lib/format";
import { readable } from "../lib/release";
import { Link } from "../router";
import { CODECS, RESOLUTIONS } from "../search/ResultRow";
import { Picture } from "../titles/Picture";
import { CHECKS, skipReason } from "../titles/words";
import { Button } from "../ui/Button";
import { Spinner } from "../ui/Spinner";
import type { Attempt, Passed } from "./titleSession";

/** The title's picture behind the screens before playback, dark enough for text at any point of it. */
function Backdrop({ src }: { src: string | null }) {
  return (
    <div aria-hidden="true" className="absolute inset-0 overflow-hidden bg-radial-[at_15%_95%] from-raised to-black to-70%">
      <div className="absolute inset-0 opacity-40">
        <Picture src={src} alt="" className="size-full object-cover" />
      </div>
      <div className="absolute inset-0 bg-linear-to-t from-black via-black/55 to-black/25" />
      <div className="absolute inset-0 bg-linear-to-r from-black/80 via-black/30 to-transparent" />
    </div>
  );
}

/** What plays: the title, and under it an episode's number and name or a film's year. */
export interface Naming {
  title: string | null;
  detail: string | null;
}

function Name({ naming }: { naming: Naming }) {
  if (!naming.title) return null;
  return (
    <div className="flex flex-col gap-2">
      <h1 className="text-3xl font-semibold tracking-tight text-balance text-ink sm:text-4xl">{naming.title}</h1>
      {naming.detail && <p className="text-base text-soft">{naming.detail}</p>}
    </div>
  );
}

/** A release in a few words: its quality, size, and swarm. */
function about(release: Release): string {
  const { quality } = release;
  return [
    quality.resolution && RESOLUTIONS[quality.resolution],
    quality.codec && CODECS[quality.codec],
    quality.hdr && "HDR",
    release.size !== null && bytes(release.size),
    release.seeders !== null && count(release.seeders, "seeder"),
    release.sources[0],
  ]
    .filter(Boolean)
    .join(" · ");
}

function PassedList({ title, passed, kind }: { title: string; passed: Passed[]; kind: "film" | "series" }) {
  if (passed.length === 0) return null;
  return (
    <section aria-label={title} className="flex flex-col gap-2">
      <h2 className="text-sm font-medium text-pencil">{title}</h2>
      <ul role="list" className="flex flex-col gap-2">
        {passed.map((source) => {
          const name = source.name ? readable(source.name) : null;
          return (
            <li key={source.release} className="flex min-w-0 flex-col text-sm">
              <p className="truncate text-soft" title={source.name ?? undefined}>
                {name ? (
                  <>
                    {name.title} <span className="text-pencil">{name.tags}</span>
                  </>
                ) : (
                  `Source ${source.release.slice(0, 8)}`
                )}
              </p>
              <p className="text-pretty text-pencil">{skipReason(source.reason, kind)}</p>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

interface Step {
  title: string;
  detail: ReactNode;
}

function steps(attempt: Attempt): Step[] {
  const { found, trying } = attempt;
  return [
    {
      title: found === null ? "Finding sources" : found === 1 ? "Found 1 source" : `Found ${found} sources`,
      detail: "Searching the indexes for this title's torrents.",
    },
    {
      title: trying ? `Trying source ${trying.rank} of ${trying.of}` : "Trying sources",
      detail: trying && (
        <>
          <span className="text-soft">{about(trying.release)}</span>
          {trying.check && <span className="block">{CHECKS[trying.check]}</span>}
        </>
      ),
    },
    { title: "Reading the video", detail: "Downloading its first pieces to see what is inside." },
    { title: "Getting ready to play", detail: "Buffering the first seconds." },
  ];
}

interface StartingProps {
  attempt: Attempt;
  naming: Naming;
  backdrop: string | null;
  /** The step in progress: 0 finding sources, 1 trying them, 2 reading the video, 3 buffering. */
  step: 0 | 1 | 2 | 3;
  /** How the download that plays is doing, under the steps. */
  readout?: ReactNode;
}

/** The screen before the first frame of a title: what plays, and what the engine is doing toward it. */
export function TitleStarting({ attempt, naming, backdrop, step, readout }: StartingProps) {
  const playing = step >= 2 && attempt.trying ? `Playing source ${attempt.trying.rank} of ${attempt.trying.of}` : null;
  return (
    <div className="absolute inset-0">
      <Backdrop src={backdrop} />
      <div className="absolute inset-0 flex items-end overflow-y-auto px-[max(--spacing(6),7vw)] pt-20 pb-[max(--spacing(8),11vh)]">
        <div className="flex w-full max-w-xl animate-appear flex-col gap-8 motion-reduce:animate-none">
          <Name naming={naming} />
          <ol role="list" aria-label="Starting" className="flex flex-col gap-3">
            {steps(attempt).map((stage, at) => (
              <li
                key={at}
                aria-current={at === step ? "step" : undefined}
                data-state={at < step ? "done" : at === step ? "now" : "next"}
                className="group/step flex items-start gap-3 text-base text-pencil data-[state=done]:text-soft data-[state=now]:text-ink"
              >
                <div className="flex h-lh w-5 shrink-0 items-center justify-center">
                  {at < step && <CheckIcon className="size-4 fill-action" />}
                  {at === step && <Spinner className="size-4 text-ink" />}
                  {at > step && <div className="size-1.5 rounded-full bg-rule" />}
                </div>
                <div className="flex min-w-0 flex-col gap-0.5">
                  <p className="group-data-[state=now]/step:font-medium">{at === 1 && playing ? playing : stage.title}</p>
                  {at === step && stage.detail && <p className="max-w-[52ch] text-sm text-pretty text-pencil tabular-nums">{stage.detail}</p>}
                </div>
              </li>
            ))}
          </ol>
          {readout && <div className="min-h-5 text-sm text-soft">{readout}</div>}
          <PassedList title="Passed over" passed={attempt.passed} kind={attempt.episode ? "series" : "film"} />
        </div>
      </div>
    </div>
  );
}

interface FailedProps {
  naming: Naming;
  backdrop: string | null;
  kind: "film" | "episode";
  error: ApiError;
  tried: Passed[];
  /** The page that lists this title's sources. */
  sources: string;
  /** Plays without the pinned source, when one was pinned. */
  unpinned: string | null;
  onRetry: () => void;
  onBack: () => void;
}

/** No source of a title played. Says why in the engine's words, and what each source did. */
export function TitleFailed({ naming, backdrop, kind, error, tried, sources, unpinned, onRetry, onBack }: FailedProps) {
  return (
    <div role="alert" className="absolute inset-0">
      <Backdrop src={backdrop} />
      <div className="absolute inset-0 flex items-end overflow-y-auto px-[max(--spacing(6),7vw)] pt-20 pb-[max(--spacing(8),11vh)]">
        <div className="flex w-full max-w-xl animate-appear flex-col gap-8 motion-reduce:animate-none">
          <Name naming={naming} />
          <div className="flex flex-col gap-2">
            <h2 className="text-lg font-medium text-balance text-ink">This {kind} cannot play right now</h2>
            <p className="text-base text-pretty text-soft sm:text-sm">{sentence(error.message)}</p>
            {error.hint && <p className="text-base text-pretty text-pencil sm:text-sm">{sentence(error.hint)}</p>}
          </div>
          <PassedList title="Tried" passed={tried} kind={kind === "film" ? "film" : "series"} />
          <div className="flex flex-wrap gap-2">
            <Button variant="primary" onClick={onRetry}>
              Try again
            </Button>
            {unpinned ? (
              <Link href={unpinned} replace className="rounded-lg px-3 py-1.5 text-sm font-medium text-ink inset-ring inset-ring-white/15 hover:bg-white/10">
                Let torrential choose
              </Link>
            ) : (
              <Link href={sources} className="rounded-lg px-3 py-1.5 text-sm font-medium text-ink inset-ring inset-ring-white/15 hover:bg-white/10">
                Choose a source
              </Link>
            )}
            <Button variant="quiet" onClick={onBack}>
              Back
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}
