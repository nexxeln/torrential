import { MagnifyingGlassIcon, XMarkIcon } from "@heroicons/react/16/solid";
import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import type { Release, SearchResult, SourceReport } from "../api";
import { count, sentence } from "../lib/format";
import { isVideo } from "../lib/release";
import { navigate, searchPath, watchPath } from "../router";
import { Header } from "../ui/Header";
import { Spinner } from "../ui/Spinner";
import { Recent } from "./Recent";
import { ResultRow, useCopied } from "./ResultRow";
import { type SearchState, useSearch } from "./useSearch";

/** Typing pauses this long before a search starts. */
const TYPING_MS = 300;
/** Text that names one torrent instead of describing one: a magnet link, an infohash, or a .torrent URL. */
const TORRENT = /^(magnet:\?\S+|[0-9a-f]{40}|https?:\/\/\S+\.torrent(\?\S*)?)$/i;

function list(names: string[]): string {
  return new Intl.ListFormat("en", { style: "long", type: "conjunction" }).format(names);
}

function failures(sources: SourceReport[]): string | null {
  const silent = sources.filter((report) => report.outcome !== "ok" && report.outcome !== "cached" && report.outcome !== "irrelevant");
  return silent.length ? `${list(silent.map((report) => report.source))} did not answer` : null;
}

function describe(state: SearchState): string {
  switch (state.kind) {
    case "idle":
    case "failed":
      return "";
    case "searching": {
      const waiting = state.pending.length ? `waiting for ${list(state.pending)}` : "searching";
      if (state.stale || !state.result) return "Searching";
      return `${count(state.result.releases.length, "result")} so far, ${waiting}`;
    }
    case "done": {
      const { releases, total, sources, hidden_adult: hidden } = state.result;
      const shown = total > releases.length ? `${releases.length} of ${count(total, "result")}` : count(releases.length, "result");
      const notes = [
        releases.length > 1 ? `${shown}, ranked for this machine` : shown,
        hidden ? `${hidden} adult hidden` : null,
        failures(sources),
      ];
      return notes.filter(Boolean).join(" · ");
    }
  }
}

function Results({ result, stale, onOpen }: { result: SearchResult; stale: boolean; onOpen: (release: Release) => void }) {
  const [chosen, setChosen] = useState<string | null>(null);
  const [copied, copy] = useCopied();
  const { releases } = result;
  // The selection follows its release as later answers reorder the list, and starts at the best match.
  const selected = releases.find((release) => release.id === chosen) ?? releases[0] ?? null;
  const activate = (release: Release) => (isVideo(release) ? onOpen(release) : copy(release));

  useEffect(() => {
    const pressed = (event: globalThis.KeyboardEvent) => {
      if (event.metaKey || event.ctrlKey || event.altKey || !selected) return;
      const at = releases.indexOf(selected);
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const step = event.key === "ArrowDown" ? 1 : -1;
        setChosen(releases[Math.min(releases.length - 1, Math.max(0, at + step))]!.id);
      } else if (event.key === "Enter" && !(event.target as Element).closest("a, button")) {
        event.preventDefault();
        activate(selected);
      }
    };
    addEventListener("keydown", pressed);
    return () => removeEventListener("keydown", pressed);
  });

  return (
    <ol role="list" aria-label="Results" aria-busy={stale} className="-mx-3 flex flex-col gap-px aria-busy:opacity-50">
      {releases.map((release) => (
        <ResultRow
          key={release.id}
          release={release}
          selected={release === selected}
          copied={copied === release.id}
          onPoint={() => setChosen(release.id)}
          onCopy={() => copy(release)}
        />
      ))}
    </ol>
  );
}

interface Props {
  /** The query in the address bar. */
  query: string;
  /** Search every category, not only video. */
  everything: boolean;
}

export function SearchPage({ query, everything }: Props) {
  const input = useRef<HTMLInputElement>(null);
  const [text, setText] = useState(query);
  const committed = useRef(query);
  const state = useSearch(query, everything);

  // Back and forward change the query from outside the field.
  useEffect(() => {
    if (query !== committed.current) {
      committed.current = query;
      setText(query);
    }
  }, [query]);

  const commit = (words: string, scope = everything) => {
    committed.current = words;
    navigate(searchPath(words, scope), { replace: true });
  };
  const torrent = TORRENT.test(text.trim());
  useEffect(() => {
    if (torrent || text.trim() === committed.current.trim()) return;
    const timer = setTimeout(() => commit(text.trim()), TYPING_MS);
    return () => clearTimeout(timer);
  });

  useEffect(() => {
    document.title = query ? `${query} · torrential` : "torrential";
    const pressed = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "/" || event.metaKey || event.ctrlKey || event.altKey) return;
      if ((event.target as Element).closest("input, textarea, select")) return;
      event.preventDefault();
      input.current?.focus();
      input.current?.select();
    };
    addEventListener("keydown", pressed);
    return () => removeEventListener("keydown", pressed);
  }, [query]);

  const typed = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Escape" && text) {
      event.preventDefault();
      setText("");
      commit("");
    } else if (event.key === "Enter" && torrent) {
      event.stopPropagation();
      navigate(watchPath(text.trim()));
    } else if (event.key === "Enter" && text.trim() !== committed.current.trim()) {
      // Enter before the typing pause ends searches now, instead of opening a result of the previous query.
      event.stopPropagation();
      commit(text.trim());
    }
  };
  const open = (release: Release) => navigate(watchPath(release.id));

  const result = state.kind === "searching" || state.kind === "done" ? state.result : null;
  const busy = state.kind === "searching";

  return (
    <div className="mx-auto flex min-h-dvh max-w-4xl flex-col px-4 pb-16 sm:px-6">
      <Header current="search" />
      <main className="flex flex-col gap-3 pt-[min(8vh,--spacing(16))]">
        <search className="sticky top-0 z-10 -mx-1 flex flex-col gap-2 bg-paper px-1 pt-3 pb-2">
          <div className="grid grid-cols-[--spacing(11)_1fr_auto] items-center">
            <input
              ref={input}
              type="search"
              name="q"
              aria-label="Search torrents"
              placeholder="Search for a film, a show, anything"
              autoFocus
              autoComplete="off"
              spellCheck={false}
              enterKeyHint="search"
              value={text}
              onChange={(event) => setText(event.target.value)}
              onKeyDown={typed}
              className="col-span-full row-start-1 h-12 w-full appearance-none rounded-xl bg-raised pr-12 pl-11 text-base text-ink inset-ring inset-ring-white/10 placeholder:text-pencil focus:outline-2 focus:-outline-offset-1 focus:outline-action/70 [&::-webkit-search-cancel-button]:hidden"
            />
            <div className="pointer-events-none col-start-1 row-start-1 flex justify-center">
              {busy ? <Spinner className="size-4 text-soft" /> : <MagnifyingGlassIcon className="size-4 fill-pencil" />}
            </div>
            <div className="col-start-3 row-start-1 flex items-center pr-2">
              {text ? (
                <button
                  type="button"
                  aria-label="Clear the search"
                  onClick={() => (setText(""), commit(""), input.current?.focus())}
                  className="relative flex size-8 items-center justify-center rounded-lg hover:bg-white/10"
                >
                  <XMarkIcon className="size-4 fill-soft" />
                  <span className="absolute top-1/2 left-1/2 size-[max(100%,3rem)] -translate-1/2 pointer-fine:hidden" aria-hidden="true" />
                </button>
              ) : (
                <kbd className="pointer-events-none mr-1.5 rounded-md px-1.5 py-0.5 font-sans text-xs font-medium text-pencil inset-ring inset-ring-white/10 max-sm:hidden">
                  /
                </kbd>
              )}
            </div>
          </div>
          <div className="flex min-h-7 items-center justify-between gap-4 px-0.5">
            <p aria-live="polite" className="truncate text-sm text-pencil tabular-nums">
              {torrent ? "Press Enter to play this torrent" : describe(state)}
            </p>
            {query && (
              <fieldset className="flex shrink-0 items-center gap-1 text-sm">
                <legend className="sr-only">What to search</legend>
                {[false, true].map((all) => (
                  <label
                    key={String(all)}
                    className="cursor-pointer rounded-md px-2 py-0.5 font-medium text-pencil hover:text-ink has-checked:bg-white/8 has-checked:text-ink has-focus-visible:outline-2 has-focus-visible:outline-action"
                  >
                    <input
                      type="radio"
                      name="scope"
                      className="sr-only"
                      checked={everything === all}
                      onChange={() => commit(text.trim(), all)}
                    />
                    {all ? "Everything" : "Video"}
                  </label>
                ))}
              </fieldset>
            )}
          </div>
        </search>

        {state.kind === "idle" && <Recent />}
        {state.kind === "failed" && (
          <div role="alert" className="flex max-w-[60ch] flex-col gap-1">
            <p className="text-base font-medium text-ink sm:text-sm">The search failed</p>
            <p className="text-base text-pretty text-soft sm:text-sm">{sentence(state.error.message)}</p>
            {state.error.hint && <p className="text-base text-pretty text-pencil sm:text-sm">{sentence(state.error.hint)}</p>}
          </div>
        )}
        {result && result.releases.length > 0 && (
          <Results result={result} stale={state.kind === "searching" && state.stale} onOpen={open} />
        )}
        {state.kind === "done" && state.result.releases.length === 0 && (
          <div className="flex max-w-[60ch] flex-col gap-1">
            <p className="text-base font-medium text-ink sm:text-sm">Nothing found for “{query}”</p>
            <p className="text-base text-pretty text-pencil sm:text-sm">
              {everything
                ? "Try fewer or different words."
                : "Try fewer or different words, or search everything instead of only video."}
            </p>
          </div>
        )}
        {result && result.releases.length > 0 && (
          <p className="pt-4 text-sm text-pencil max-sm:hidden">
            <kbd className="font-sans font-medium text-soft">↑ ↓</kbd> to choose ·{" "}
            <kbd className="font-sans font-medium text-soft">Enter</kbd> to play ·{" "}
            <kbd className="font-sans font-medium text-soft">/</kbd> to search
          </p>
        )}
      </main>
    </div>
  );
}
