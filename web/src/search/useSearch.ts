import { useEffect, useState } from "react";
import { type ApiError, asApiError, search, type SearchResult } from "../api";

export type SearchState =
  | { kind: "idle" }
  /** Sources are answering. `result` is from the previous query until this one's first answer, which `stale` says. */
  | { kind: "searching"; result: SearchResult | null; pending: string[]; stale: boolean }
  | { kind: "done"; result: SearchResult }
  | { kind: "failed"; error: ApiError };

const LIMIT = 50;

export function useSearch(query: string, everything: boolean): SearchState {
  const [state, setState] = useState<SearchState>({ kind: "idle" });

  useEffect(() => {
    const words = query.trim();
    if (!words) return setState({ kind: "idle" });
    const stop = new AbortController();
    setState((before) => ({
      kind: "searching",
      result: before.kind === "searching" || before.kind === "done" ? before.result : null,
      pending: [],
      stale: true,
    }));
    (async () => {
      const options = { category: everything ? undefined : ("video" as const), limit: LIMIT, signal: stop.signal };
      for await (const message of search(words, options)) {
        if (stop.signal.aborted) return;
        if (message.type === "update") {
          setState({ kind: "searching", result: message.result, pending: message.pending, stale: false });
        } else if (message.type === "done") {
          setState({ kind: "done", result: message.result });
        } else {
          setState({ kind: "failed", error: message.error });
        }
      }
    })().catch((thrown) => {
      if (!stop.signal.aborted) setState({ kind: "failed", error: asApiError(thrown) });
    });
    return () => stop.abort();
  }, [query, everything]);

  return state;
}
