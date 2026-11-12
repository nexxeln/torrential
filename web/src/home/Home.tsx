import { type KeyboardEvent, type RefObject, useEffect, useRef, useState } from "react";
import { type ApiError, artUrl, asApiError, type ContinueItem, type Home as HomeData, home } from "../api";
import { count, minutes, sentence } from "../lib/format";
import { useJobs } from "../lib/jobs";
import { SearchField, useSearchText } from "../search/SearchField";
import { homePath, Link, navigate, playPath, titlePath, torrentsPath } from "../router";
import { Picture } from "../titles/Picture";
import { TitleGrid } from "../titles/TitleGrid";
import { type TitlesState, useTitles } from "../titles/useTitles";
import { episodeCode, episodeName } from "../titles/words";
import { Header } from "../ui/Header";
import { Meter } from "../ui/Meter";

function describe(state: TitlesState): string {
  switch (state.kind) {
    case "idle":
    case "failed":
      return "";
    case "searching":
      return "Searching";
    case "done": {
      const films = state.titles.filter((card) => card.kind === "film").length;
      const shows = state.titles.length - films;
      if (state.titles.length === 0) return "";
      return [films ? count(films, "film") : null, shows ? count(shows, "show") : null].filter(Boolean).join(" and ");
    }
  }
}

/** The way to today's torrent search, for what the catalog does not list. */
function TorrentsLink({ query }: { query: string }) {
  return (
    <Link href={torrentsPath(query, false)} className="self-start rounded-sm text-sm text-pencil hover:text-ink">
      Search torrents for “{query}”
    </Link>
  );
}

function Results({ query, state, onLeaveTop, grid }: { query: string; state: TitlesState; onLeaveTop: () => void; grid: RefObject<HTMLOListElement | null> }) {
  if (state.kind === "failed") {
    return (
      <div role="alert" className="flex max-w-[60ch] flex-col gap-3">
        <div className="flex flex-col gap-1">
          <p className="text-base font-medium text-ink sm:text-sm">The search failed</p>
          <p className="text-base text-pretty text-soft sm:text-sm">{sentence(state.error.message)}</p>
          {state.error.hint && <p className="text-base text-pretty text-pencil sm:text-sm">{sentence(state.error.hint)}</p>}
        </div>
        <TorrentsLink query={query} />
      </div>
    );
  }
  const shown = state.kind === "searching" || state.kind === "done" ? state.titles : null;
  if (state.kind === "done" && state.titles.length === 0) {
    return (
      <div className="flex max-w-[60ch] flex-col gap-3">
        <p className="text-base text-pretty text-soft sm:text-sm">The catalog lists no film or show called “{query}”.</p>
        <TorrentsLink query={query} />
      </div>
    );
  }
  if (!shown?.length) return null;
  return (
    <div className="flex flex-col gap-8 pt-2">
      <TitleGrid list={grid} titles={shown} label="Films and shows" stale={state.kind === "searching"} onLeaveTop={onLeaveTop} />
      <div className="flex flex-col gap-4">
        <TorrentsLink query={query} />
        <p className="text-sm text-pencil max-sm:hidden">
          <kbd className="font-sans font-medium text-soft">↑ ↓ ← →</kbd> to choose ·{" "}
          <kbd className="font-sans font-medium text-soft">Enter</kbd> to open ·{" "}
          <kbd className="font-sans font-medium text-soft">/</kbd> to search
        </p>
      </div>
    </div>
  );
}

function ContinueCard({ item }: { item: ContinueItem }) {
  const { title, episode } = item;
  const background = (
    <Picture
      src={artUrl(title.id, "background")}
      alt=""
      className="absolute inset-0 size-full object-cover"
      fallback={
        <div aria-hidden="true" className="absolute inset-0 flex items-end bg-linear-to-br from-white/8 to-raised p-4">
          <p className="line-clamp-2 text-lg font-medium text-balance text-soft">{title.name}</p>
        </div>
      }
    />
  );
  const started = item.resume_at !== null && item.duration !== null && item.duration > 0;
  const left = started ? `${minutes((item.duration! - item.resume_at!) / 60)} left` : episode ? "Next episode" : "Not started";
  const what = episode ? `${episodeCode(episode)} · ${episodeName(episode)}` : title.year;
  return (
    <li className="snap-start">
      <Link
        href={playPath(item.video)}
        aria-label={[title.name, what, left].filter(Boolean).join(", ")}
        className="group/card flex flex-col gap-2.5 rounded-lg"
      >
        <div className="relative aspect-video overflow-hidden rounded-lg bg-raised outline-1 -outline-offset-1 outline-white/10 group-hover/card:outline-white/30">
          {episode?.thumbnail ? <Picture src={artUrl(title.id, episode)} alt="" className="absolute inset-0 size-full object-cover" fallback={background} /> : background}
          {started && (
            <div className="absolute inset-x-0 bottom-0 bg-linear-to-t from-black/70 to-transparent px-3 pt-6 pb-3">
              <Meter fraction={item.resume_at! / item.duration!} label="Watched" />
            </div>
          )}
        </div>
        <div className="grid grid-cols-[minmax(0,1fr)_auto] items-baseline gap-x-3 px-0.5">
          <p className="truncate text-base font-medium text-ink sm:text-sm">{title.name}</p>
          <p className="text-sm text-pencil tabular-nums">{left}</p>
          <p className="col-span-full truncate text-sm text-pencil tabular-nums">{what}</p>
        </div>
      </Link>
    </li>
  );
}

type HomeState = { kind: "loading" } | { kind: "ready"; home: HomeData } | { kind: "failed"; error: ApiError };

/** What there is to pick up again, and the titles saved for later. */
function Shelf() {
  const [state, setState] = useState<HomeState>({ kind: "loading" });
  // Asks again when the engine comes back.
  const live = useJobs().link === "live";
  useEffect(() => {
    const stop = new AbortController();
    home(stop.signal).then(
      (found) => setState({ kind: "ready", home: found }),
      (thrown) => !stop.signal.aborted && setState({ kind: "failed", error: asApiError(thrown) }),
    );
    return () => stop.abort();
  }, [live]);

  if (state.kind === "loading") return null;
  if (state.kind === "failed") {
    return (
      <p className="max-w-[60ch] text-base text-pretty text-pencil sm:text-sm">
        {sentence(state.error.message)} {state.error.hint && sentence(state.error.hint)}
      </p>
    );
  }
  const { continue: unfinished, saved } = state.home;
  if (unfinished.length === 0 && saved.length === 0) {
    return <p className="max-w-[60ch] text-base text-pretty text-pencil sm:text-sm">Search for a film or a show to watch. It plays here while it downloads.</p>;
  }
  return (
    <div className="flex flex-col gap-10 pt-2">
      {unfinished.length > 0 && (
        <section aria-labelledby="continue" className="flex flex-col gap-3">
          <h2 id="continue" className="text-sm font-medium text-pencil">
            Continue watching
          </h2>
          <ul
            role="list"
            className="-mx-4 grid snap-x snap-mandatory scroll-px-4 auto-cols-[78%] grid-flow-col gap-x-3 overflow-x-auto px-4 pb-2 sm:mx-0 sm:snap-none sm:auto-cols-auto sm:grid-flow-row sm:grid-cols-2 sm:gap-x-4 sm:gap-y-6 sm:overflow-visible sm:px-0 sm:pb-0 lg:grid-cols-3"
          >
            {unfinished.map((item) => (
              <ContinueCard key={item.video} item={item} />
            ))}
          </ul>
        </section>
      )}
      {saved.length > 0 && (
        <section aria-labelledby="saved" className="flex flex-col gap-3">
          <h2 id="saved" className="text-sm font-medium text-pencil">
            Saved
          </h2>
          <TitleGrid titles={saved} label="Saved" />
        </section>
      )}
    </div>
  );
}

/** The first page: a search for films and shows, and under it what to watch next. */
export function Home({ query }: { query: string }) {
  const input = useRef<HTMLInputElement>(null);
  const grid = useRef<HTMLOListElement>(null);
  const state = useTitles(query);
  const search = useSearchText(query, (words) => navigate(homePath(words), { replace: true }));

  useEffect(() => {
    document.title = query ? `${query} · torrential` : "torrential";
  }, [query]);

  const keyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (search.keyDown(event)) return;
    const first = state.kind === "done" ? state.titles[0] : undefined;
    if (event.key === "ArrowDown" && grid.current) {
      event.preventDefault();
      grid.current.querySelector("a")?.focus();
    } else if (event.key === "Enter" && first) {
      navigate(titlePath(first.id));
    }
  };

  return (
    <div className="mx-auto flex min-h-dvh max-w-4xl flex-col px-4 pb-16 sm:px-6">
      <Header current="search" />
      <main className="flex flex-col gap-3 pt-[min(8vh,--spacing(16))]">
        <SearchField
          input={input}
          search={search}
          label="Search films and shows"
          placeholder="Search for a film or a show"
          busy={state.kind === "searching"}
          onKeyDown={keyDown}
          status={describe(state)}
        />
        {query.trim() ? <Results query={query.trim()} state={state} grid={grid} onLeaveTop={() => input.current?.focus()} /> : <Shelf />}
      </main>
    </div>
  );
}
