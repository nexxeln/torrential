import { CheckIcon } from "@heroicons/react/16/solid";
import { artUrl, type EpisodeInfo, type TitlePage, type Watched } from "../api";
import { day, minutes } from "../lib/format";
import { Link, playPath } from "../router";
import { Meter } from "../ui/Meter";
import { Blank, Picture } from "./Picture";
import { episodeName, hasAired, seasonName, videoOf } from "./words";

/** The regular seasons in order, then the specials. */
export function seasonOrder(page: TitlePage): number[] {
  const numbers = page.seasons.map((season) => season.number);
  return [...numbers.filter((n) => n > 0), ...numbers.filter((n) => n === 0)];
}

function facts(episode: EpisodeInfo, watched: Watched | undefined, next: boolean): string {
  if (!hasAired(episode)) return episode.aired === null ? "Not announced yet" : `Airs ${day(episode.aired)}`;
  const left = watched && !watched.finished && watched.duration ? `${minutes((watched.duration - watched.position) / 60)} left` : null;
  return [day(episode.aired!), left, next ? "Up next" : null].filter(Boolean).join(" · ");
}

const ROW = "grid grid-cols-[--spacing(32)_minmax(0,1fr)] items-start gap-x-4 gap-y-2 rounded-lg p-2 sm:grid-cols-[--spacing(44)_minmax(0,1fr)]";

function EpisodeRow({ id, episode, watched, next }: { id: string; episode: EpisodeInfo; watched: Watched | undefined; next: boolean }) {
  const aired = hasAired(episode);
  const started = watched && !watched.finished && watched.duration ? watched.position / watched.duration : null;
  const body = (
    <>
      <div className="relative aspect-video overflow-hidden rounded-md bg-raised outline-1 -outline-offset-1 outline-white/10 sm:row-span-2">
        <Picture
          src={episode.thumbnail ? artUrl(id, episode) : null}
          alt=""
          className="absolute inset-0 size-full object-cover"
          fallback={<Blank kind="series" />}
        />
        {started !== null && (
          <div className="absolute inset-x-0 bottom-0 bg-linear-to-t from-black/70 to-transparent px-2 pt-4 pb-2">
            <Meter fraction={started} label="Watched" />
          </div>
        )}
      </div>
      <div className="flex min-w-0 flex-col gap-0.5 sm:pt-0.5">
        <div className="flex items-start gap-2">
          <p className="min-w-0 flex-1 text-base font-medium text-pretty text-ink sm:text-sm">
            <span className="text-pencil tabular-nums">{episode.episode}.</span> {episodeName(episode)}
          </p>
          {watched?.finished && <CheckIcon aria-label="Watched" className="h-lh w-4 shrink-0 fill-action" />}
        </div>
        <p className="text-sm text-pencil tabular-nums">{facts(episode, watched, next)}</p>
      </div>
      {episode.overview && (
        <p className="line-clamp-2 text-base text-pretty text-soft max-sm:col-span-full sm:col-start-2 sm:text-sm">{episode.overview}</p>
      )}
    </>
  );
  return (
    <li>
      {aired ? (
        <Link href={playPath(videoOf(id, episode))} aria-current={next ? "true" : undefined} className={`${ROW} hover:bg-white/5`}>
          {body}
        </Link>
      ) : (
        <div aria-disabled="true" title="This episode has not aired" className={`${ROW} opacity-50`}>
          {body}
        </div>
      )}
    </li>
  );
}

interface Props {
  page: TitlePage;
  season: number;
  onSeason: (season: number) => void;
  /** The episode Play plays. */
  next: string | null;
}

/** A series' episodes, one season at a time. */
export function Episodes({ page, season, onSeason, next }: Props) {
  const shown = page.seasons.find((s) => s.number === season) ?? page.seasons[0];
  const watched = new Map(page.watched.map((entry) => [entry.video, entry]));
  if (!shown) return null;
  return (
    <section aria-labelledby="episodes" className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-4">
        <h2 id="episodes" className="text-lg font-medium text-ink">
          Episodes
        </h2>
        <div className="inline-grid grid-cols-[1fr_--spacing(8)] text-ink">
          <select
            name="season"
            aria-label="Season"
            value={shown.number}
            onChange={(event) => onSeason(Number(event.target.value))}
            className="col-span-full row-start-1 appearance-none rounded-lg bg-raised py-2 pr-8 pl-3 text-base font-medium [color-scheme:dark] inset-ring inset-ring-white/10 hover:bg-white/10 focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-action sm:py-1.5 sm:text-sm pointer-coarse:min-h-11 tv:focus:outline-[0.1875rem] tv:focus:outline-ink"
          >
            {seasonOrder(page).map((number) => (
              <option key={number} value={number}>
                {seasonName(number)}
              </option>
            ))}
          </select>
          <svg viewBox="0 0 8 5" width="8" height="5" fill="none" aria-hidden="true" className="pointer-events-none col-start-2 row-start-1 place-self-center">
            <path d="M.5.5 4 4 7.5.5" stroke="currentcolor" />
          </svg>
        </div>
      </div>
      <ol role="list" aria-label={seasonName(shown.number)} className="-mx-2 flex flex-col gap-1">
        {shown.episodes.map((episode) => {
          const video = videoOf(page.id, episode);
          return <EpisodeRow key={video} id={page.id} episode={episode} watched={watched.get(video)} next={video === next} />;
        })}
      </ol>
    </section>
  );
}
