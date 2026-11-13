import { useEffect, useRef, useState } from "react";
import type { Release, SearchResult, SourceReport } from "../api";
import { count, list, sentence } from "../lib/format";
import { isVideo } from "../lib/release";
import { navigate, torrentsPath, watchPath } from "../router";
import { Header } from "../ui/Header";
import { Recent } from "./Recent";
import { ResultRow, useCopied } from "./ResultRow";
import { SearchField, useSearchText } from "./SearchField";
import { type SearchState, useSearch } from "./useSearch";

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
  const state = useSearch(query, everything);
  const search = useSearchText(query, (words) => navigate(torrentsPath(words, everything), { replace: true }));
  const { text } = search;

  useEffect(() => {
    document.title = query ? `${query} · torrential` : "torrential";
  }, [query]);

  const open = (release: Release) => navigate(watchPath(release.id));

  const result = state.kind === "searching" || state.kind === "done" ? state.result : null;
  const busy = state.kind === "searching";

  return (
    <div className="mx-auto flex min-h-dvh max-w-4xl flex-col px-4 pb-16 sm:px-6">
      <Header current="search" />
      <main className="flex flex-col gap-3 pt-[min(8vh,--spacing(16))]">
        <SearchField
          input={input}
          search={search}
          label="Search torrents"
          placeholder="Search for a film, a show, anything"
          busy={busy}
          onKeyDown={search.keyDown}
          status={describe(state)}
          aside={
            query && (
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
                      onChange={() => navigate(torrentsPath(text.trim(), all), { replace: true })}
                    />
                    {all ? "Everything" : "Video"}
                  </label>
                ))}
              </fieldset>
            )
          }
        />

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
