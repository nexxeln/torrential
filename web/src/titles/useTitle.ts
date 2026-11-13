import { useEffect, useState } from "react";
import { type ApiError, asApiError, title, type TitlePage } from "../api";

export type TitleState = { kind: "loading" } | { kind: "ready"; page: TitlePage } | { kind: "failed"; error: ApiError };

/** Pages read in this visit, so going back to one shows it at once while the engine is asked again. */
const kept = new Map<string, TitlePage>();

/** One title as the engine describes it, and a way to show a change to it at once. */
export function useTitle(id: string): [TitleState, (page: TitlePage) => void] {
  const [state, setState] = useState<TitleState>(() => {
    const page = kept.get(id);
    return page ? { kind: "ready", page } : { kind: "loading" };
  });

  useEffect(() => {
    const stop = new AbortController();
    title(id, stop.signal).then(
      (page) => {
        kept.set(id, page);
        setState({ kind: "ready", page });
      },
      (thrown) => {
        if (!stop.signal.aborted) setState((before) => (before.kind === "ready" ? before : { kind: "failed", error: asApiError(thrown) }));
      },
    );
    return () => stop.abort();
  }, [id]);

  const update = (page: TitlePage) => {
    kept.set(id, page);
    setState({ kind: "ready", page });
  };
  return [state, update];
}
