import { type DependencyList, useEffect } from "react";
import { type ApiError, asApiError } from "../api";

/**
 * Asks with `ask` each time `deps` change, and gives `answered` the answer or `failed` the error.
 * A newer ask stops the one before, whose answer then counts for nothing.
 */
export function useLatest<T>(
  ask: (signal: AbortSignal) => Promise<T>,
  answered: (value: T) => void,
  failed: (error: ApiError) => void,
  deps: DependencyList,
): void {
  useEffect(() => {
    const stop = new AbortController();
    ask(stop.signal).then(
      (value) => !stop.signal.aborted && answered(value),
      (thrown) => !stop.signal.aborted && failed(asApiError(thrown)),
    );
    return () => stop.abort();
  }, deps);
}
