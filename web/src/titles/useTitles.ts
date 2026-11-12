import { useEffect, useState } from "react";
import { type ApiError, asApiError, type TitleCard, titles } from "../api";

export type TitlesState =
  | { kind: "idle" }
  /** The catalog is answering. `titles` are the previous query's until then. */
  | { kind: "searching"; titles: TitleCard[] | null }
  | { kind: "done"; titles: TitleCard[] }
  | { kind: "failed"; error: ApiError };

/** The films and shows the catalog names like `query`. */
export function useTitles(query: string): TitlesState {
  const [state, setState] = useState<TitlesState>({ kind: "idle" });

  useEffect(() => {
    const words = query.trim();
    if (!words) return setState({ kind: "idle" });
    const stop = new AbortController();
    setState((before) => ({ kind: "searching", titles: before.kind === "searching" || before.kind === "done" ? before.titles : null }));
    titles(words, stop.signal).then(
      (found) => setState({ kind: "done", titles: found }),
      (thrown) => {
        if (!stop.signal.aborted) setState({ kind: "failed", error: asApiError(thrown) });
      },
    );
    return () => stop.abort();
  }, [query]);

  return state;
}
