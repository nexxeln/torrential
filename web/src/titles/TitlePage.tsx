import { BookmarkIcon, PlayIcon, StarIcon } from "@heroicons/react/16/solid";
import { Fragment, type ReactNode, useEffect } from "react";
import { artUrl, type Next, setSaved, type TitlePage as Title } from "../api";
import { count, list, minutes, sentence } from "../lib/format";
import { Link, navigate, playPath, titlePath } from "../router";
import { CastButton } from "../cast/CastButton";
import { Button, buttonClass } from "../ui/Button";
import { Header } from "../ui/Header";
import { Spinner } from "../ui/Spinner";
import { Episodes, seasonOrder } from "./Episodes";
import { Logo } from "./Logo";
import { Picture } from "./Picture";
import { Sources } from "./Sources";
import { useTitle } from "./useTitle";
import { episodeCode, findEpisode, playingOrder, videoOf } from "./words";

/** What the page's Play button plays. */
interface Primary {
  video: string;
  label: string;
  /** How much is left of a film or an episode that was started. */
  left: string | null;
}

/** A finished film, or a series that is caught up, starts over. */
function restart(page: Title): Next | null {
  if (page.kind === "film") return { video: page.id, resume_at: null };
  const first = playingOrder(page, false)[0];
  return first ? { video: videoOf(page.id, first), resume_at: null } : null;
}

function primaryOf(page: Title): Primary | null {
  const target = page.next ?? restart(page);
  if (!target) return null;
  const episode = findEpisode(page, target.video);
  const resuming = target.resume_at !== null && target.resume_at > 0;
  const verb = resuming ? "Resume" : "Play";
  const duration = page.watched.find((entry) => entry.video === target.video)?.duration ?? (page.runtime_min ? page.runtime_min * 60 : null);
  return {
    video: target.video,
    label: episode ? `${verb} ${episodeCode(episode)}` : verb,
    left: resuming && duration ? `${minutes((duration - target.resume_at!) / 60)} left` : null,
  };
}

function Facts({ page }: { page: Title }) {
  const seasons = page.seasons.filter((season) => season.number > 0).length;
  const facts: ReactNode[] = [
    page.year,
    page.kind === "series" && seasons > 0 ? count(seasons, "season") : null,
    page.runtime_min ? minutes(page.runtime_min) : null,
    page.genres.length ? page.genres.join(", ") : null,
    page.rating !== null ? (
      <span className="inline-flex items-center gap-1" title="IMDb rating">
        <StarIcon aria-hidden="true" className="size-3.5 fill-pencil" />
        {page.rating.toFixed(1)}
      </span>
    ) : null,
  ].filter((fact) => fact !== null && fact !== undefined);
  return (
    <p className="flex flex-wrap items-center gap-x-2 text-sm text-soft tabular-nums">
      {facts.map((fact, at) => (
        <Fragment key={at}>
          {at > 0 && <span aria-hidden="true" className="text-pencil">·</span>}
          {fact}
        </Fragment>
      ))}
    </p>
  );
}

function SaveButton({ page, onChange }: { page: Title; onChange: (page: Title) => void }) {
  const toggle = () => {
    const wanted = !page.saved;
    onChange({ ...page, saved: wanted });
    setSaved(page.id, wanted).then(
      (saved) => saved !== wanted && onChange({ ...page, saved }),
      () => onChange({ ...page, saved: !wanted }),
    );
  };
  return (
    <Button size="large" aria-pressed={page.saved} onClick={toggle}>
      <BookmarkIcon aria-hidden="true" className={`size-4 ${page.saved ? "fill-action" : "fill-soft"}`} />
      {page.saved ? "Saved" : "Save"}
    </Button>
  );
}

/** The title's picture across the top of the page, fading into the page. */
function Backdrop({ page }: { page: Title }) {
  return (
    <div aria-hidden="true" className="absolute inset-x-0 top-0 -z-10 h-[min(62vh,--spacing(120))] overflow-hidden sm:h-[min(88vh,--spacing(208))]">
      {page.art.background && <Picture src={artUrl(page.id, "background")} alt="" className="size-full object-cover object-[50%_20%]" />}
      <div className="absolute inset-0 bg-linear-to-r from-paper via-paper/75 to-paper/10 max-sm:hidden" />
      <div className="absolute inset-0 bg-linear-to-t from-paper from-15% via-paper/45 to-paper/50 sm:from-0% sm:via-paper/40 sm:to-paper/60" />
      <div className="absolute inset-x-0 top-0 h-24 bg-linear-to-b from-paper/80 to-transparent" />
    </div>
  );
}

function Name({ page }: { page: Title }) {
  const name = <span className="text-4xl font-semibold tracking-tight text-balance text-ink sm:text-5xl">{page.name}</span>;
  return (
    <h1 className="flex min-h-24 items-end sm:min-h-32">
      {page.art.logo ? (
        <Logo src={artUrl(page.id, "logo")} alt={page.name} fallback={name} />
      ) : (
        name
      )}
    </h1>
  );
}

interface Props {
  id: string;
  /** The season shown, or null for the one of the episode Play plays. */
  season: number | null;
  /** The film or episode whose sources are listed and open, from a link to them. */
  sources: string | null;
}

/** One film or series: what it is, Play, its sources, and a series' episodes. */
export function TitlePage({ id, season, sources }: Props) {
  const [state, update] = useTitle(id);
  const page = state.kind === "ready" ? state.page : null;

  useEffect(() => {
    document.title = page ? `${page.name}${page.year ? ` (${page.year})` : ""} · torrential` : "torrential";
  }, [page]);

  return (
    <div className="relative isolate min-h-dvh">
      {page && <Backdrop page={page} />}
      <div className="mx-auto flex max-w-4xl flex-col px-4 pb-24 sm:px-6">
        <Header current={null} />
        {state.kind === "loading" && (
          <div className="flex justify-center pt-[30vh]">
            <Spinner className="size-5 text-soft" />
          </div>
        )}
        {state.kind === "failed" && (
          <main className="flex max-w-[60ch] flex-col items-start gap-2 pt-[min(8vh,--spacing(16))]">
            <h1 className="text-lg font-medium text-ink">This title cannot be shown</h1>
            <p className="text-base text-pretty text-soft sm:text-sm">{sentence(state.error.message)}</p>
            {state.error.hint && <p className="text-base text-pretty text-pencil sm:text-sm">{sentence(state.error.hint)}</p>}
            <Link href="/" className="mt-2 rounded-sm text-sm text-action hover:text-ink">
              Search for a film or a show
            </Link>
          </main>
        )}
        {page && <Details page={page} season={season} sources={sources} onChange={update} />}
      </div>
    </div>
  );
}

function Details({ page, season, sources, onChange }: { page: Title; season: number | null; sources: string | null; onChange: (page: Title) => void }) {
  const primary = primaryOf(page);
  const listed = sources ?? primary?.video ?? null;
  const listedEpisode = listed ? findEpisode(page, listed) : null;
  const seasonOf = (video: string | null | undefined) => (video ? (findEpisode(page, video)?.season ?? null) : null);
  const shownSeason = season ?? seasonOf(sources) ?? seasonOf(primary?.video) ?? seasonOrder(page)[0] ?? null;

  return (
    <main className="flex flex-col gap-12 pt-[min(24vh,--spacing(52))]">
      <section className="flex max-w-xl flex-col gap-5">
        <Name page={page} />
        <Facts page={page} />
        {page.synopsis && <p className="max-w-[60ch] text-base text-pretty text-soft">{page.synopsis}</p>}
        {page.cast.length > 0 && <p className="text-sm text-pretty text-pencil">Starring {list(page.cast)}</p>}
        <div className="flex flex-wrap items-center gap-2 pt-1">
          {primary ? (
            <Link href={playPath(primary.video)} className={buttonClass({ variant: "primary", size: "large" })}>
              <PlayIcon aria-hidden="true" className="size-4 fill-paper" />
              {primary.label}
              {primary.left && <span className="font-normal text-paper/65 tabular-nums">{primary.left}</span>}
            </Link>
          ) : (
            <p className="text-sm text-pencil">No episode has aired yet.</p>
          )}
          {primary && <CastButton look="page" start={{ video: primary.video }} />}
          <SaveButton page={page} onChange={onChange} />
        </div>
      </section>

      {listed && (
        <Sources
          key={listed}
          video={listed}
          label={listedEpisode ? episodeCode(listedEpisode) : null}
          kind={page.kind === "film" ? "film" : "episode"}
          open={sources !== null}
        />
      )}
      {page.kind === "series" && shownSeason !== null && (
        <Episodes
          page={page}
          season={shownSeason}
          next={primary?.video ?? null}
          onSeason={(number) => navigate(titlePath(page.id, { season: number, sources: sources ?? undefined }), { replace: true })}
        />
      )}
    </main>
  );
}
