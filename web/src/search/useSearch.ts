import { useEffect, useRef, useState } from "react";
import { type ApiError, asApiError, search, type SearchMessage, type SearchResult } from "../api";

export type SearchState =
  | { kind: "idle" }
  /** Sources are answering. `result` is from the previous query until this one's first answer, which `stale` says. */
  | { kind: "searching"; result: SearchResult | null; pending: string[]; stale: boolean }
  | { kind: "done"; result: SearchResult }
  | { kind: "failed"; error: ApiError };

const LIMIT = 50;

/** Follows the search stream that `open` starts, and starts it again whenever `key` changes. A null key is idle. */
export function useSearchStream(key: string | null, open: (signal: AbortSignal) => AsyncGenerator<SearchMessage>): SearchState {
  const [state, setState] = useState<SearchState>({ kind: "idle" });
  const opener = useRef(open);
  opener.current = open;

  useEffect(() => {
    if (key === null) return setState({ kind: "idle" });
    const stop = new AbortController();
    setState((before) => ({
      kind: "searching",
      result: before.kind === "searching" || before.kind === "done" ? before.result : null,
      pending: [],
      stale: true,
    }));
    (async () => {
      for await (const message of opener.current(stop.signal)) {
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
  }, [key]);

  return state;
}

export function useSearch(query: string, everything: boolean): SearchState {
  const words = query.trim();
  return useSearchStream(words ? `${everything}:${words}` : null, (signal) =>
    search(words, { category: everything ? undefined : "video", limit: LIMIT, signal }),
  );
}
