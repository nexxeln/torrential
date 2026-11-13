import { ChevronRightIcon } from "@heroicons/react/16/solid";
import { useEffect, useRef, useState } from "react";
import { sources } from "../api";
import { count, list, sentence } from "../lib/format";
import { playback } from "../player/plan";
import { playPath } from "../router";
import { ResultRow } from "../search/ResultRow";
import { type SearchState, useSearchStream } from "../search/useSearch";
import { skipReason } from "./words";

function describe(state: SearchState, what: string): string {
  switch (state.kind) {
    case "idle":
      return "";
    case "searching":
      return state.pending.length ? `Looking for sources, waiting for ${list(state.pending)}` : "Looking for sources";
    case "done": {
      const { releases } = state.result;
      if (releases.length === 0) return `No source of this ${what} was found.`;
      return releases.length === 1 ? "1 source" : `${count(releases.length, "source")}, in the order Play tries them`;
    }
    case "failed":
      return [sentence(state.error.message), state.error.hint && sentence(state.error.hint)].filter(Boolean).join(" ");
  }
}

interface Props {
  /** The film or episode whose sources are listed. */
  video: string;
  /** Which episode, for a series. */
  label: string | null;
  kind: "film" | "episode";
  /** Opened by a link to this list, which scrolls to it. */
  open: boolean;
}

/** The releases Play would try, listed only when opened. Each plays that release and no other. */
export function Sources({ video, label, kind, open: opened }: Props) {
  const [open, setOpen] = useState(opened);
  const state = useSearchStream(open ? video : null, (signal) => sources(video, playback(), signal));
  const result = state.kind === "searching" || state.kind === "done" ? state.result : null;

  // A link to the list scrolls to it once the first sources make the page long enough to.
  const section = useRef<HTMLElement>(null);
  const scrolled = useRef(!opened);
  const arrived = (result?.releases.length ?? 0) > 0;
  useEffect(() => {
    if (scrolled.current || !arrived) return;
    scrolled.current = true;
    section.current?.scrollIntoView({ block: "start" });
  }, [arrived]);

  return (
    <section ref={section} aria-labelledby="sources-title" className="flex scroll-mt-4 flex-col gap-2">
      <h2 id="sources-title">
        <button
          type="button"
          aria-expanded={open}
          aria-controls="sources"
          onClick={() => setOpen(!open)}
          className="group/open -mx-2 flex items-center gap-1.5 rounded-lg px-2 py-1.5 text-base font-medium text-ink hover:bg-white/5 sm:text-sm pointer-coarse:min-h-11"
        >
          <ChevronRightIcon className="size-4 fill-pencil transition-transform group-aria-expanded/open:rotate-90 motion-reduce:transition-none" />
          Sources
          {label && <span className="font-normal text-pencil">of {label}</span>}
        </button>
      </h2>
      {open && (
        <div id="sources" className="flex flex-col gap-2">
          <p aria-live="polite" className="text-sm text-pretty text-pencil tabular-nums">
            {describe(state, kind)}
          </p>
          {result && result.releases.length > 0 && (
            <ol role="list" aria-label="Sources" aria-busy={state.kind === "searching" && state.stale} className="-mx-3 flex flex-col gap-px aria-busy:opacity-50">
              {result.releases.map((release, at) => (
                <ResultRow
                  key={release.id}
                  release={release}
                  selected={false}
                  copied={false}
                  onPoint={() => {}}
                  onCopy={() => {}}
                  href={playPath(video, release.id)}
                  pick={at === 0}
                  failed={release.failed && skipReason(release.failed, kind === "film" ? "film" : "series")}
                />
              ))}
            </ol>
          )}
        </div>
      )}
    </section>
  );
}
