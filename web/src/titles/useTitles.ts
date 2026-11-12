import { useEffect, useState } from "react";
import { type ApiError, asApiError, type TitleCard, titles } from "../api";

/** What the catalog said about one query. */
export type TitlesAnswer =
  | { kind: "found"; titles: TitleCard[] }
  /** Title lookups are turned off in config.yaml. */
  | { kind: "offline" }
  | { kind: "failed"; error: ApiError };

export type TitlesState =
  | { kind: "idle" }
  /** The catalog is answering. `before` is the previous query's answer, shown until then. */
  | { kind: "searching"; before: TitlesAnswer | null }
  | TitlesAnswer;

/** Answers in this visit, so going back to a search shows it at once. */
const answered = new Map<string, TitleCard[]>();

function known(words: string): TitlesState {
  const found = answered.get(words);
  return found ? { kind: "found", titles: found } : { kind: "idle" };
}

/** The films and shows the catalog names like `query`. */
export function useTitles(query: string): TitlesState {
  const words = query.trim();
  const [state, setState] = useState<TitlesState>(() => known(words));

  useEffect(() => {
    if (!words || answered.has(words)) return setState(known(words));
    const stop = new AbortController();
    setState((before) => ({ kind: "searching", before: before.kind === "searching" ? before.before : before.kind === "idle" ? null : before }));
    titles(words, stop.signal).then(
      (answer) => {
        if (answer.offline) return setState({ kind: "offline" });
        answered.set(words, answer.titles);
        setState({ kind: "found", titles: answer.titles });
      },
      (thrown) => {
        if (!stop.signal.aborted) setState({ kind: "failed", error: asApiError(thrown) });
      },
    );
    return () => stop.abort();
  }, [words]);

  return state;
}
