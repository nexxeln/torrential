import { useEffect, useState } from "react";
import { type Version, version } from "../api";
import type { JobsView } from "../lib/jobs";

const DISMISSED = "torrential:dismissed-notice";
const EVERY = 30 * 60 * 1000;
/** The engine's version when this page loaded. It outlives the notice, which unmounts while the engine is away. */
let loaded: string | null = null;

function dismissed(): string | null {
  try {
    return localStorage.getItem(DISMISSED);
  } catch {
    return null;
  }
}

function dismiss(line: string): void {
  try {
    localStorage.setItem(DISMISSED, line);
  } catch {
    // Private windows refuse writes. The notice comes back on the next page load.
  }
}

/**
 * One quiet line about versions: a newer torrential, or an update the engine installed. An engine
 * that restarted into a new version under this page asks for a reload, since the page is the old one.
 */
export function UpdateNotice({ link }: { link: JobsView["link"] }) {
  const [latest, setLatest] = useState<Version | null>(null);
  const [hidden, setHidden] = useState(dismissed);

  // The event stream comes back live after the engine restarts, so the version is asked again then.
  useEffect(() => {
    if (link !== "live") return;
    const controller = new AbortController();
    const ask = () =>
      version(controller.signal).then((answer) => {
        loaded ??= answer.version;
        setLatest(answer);
      }, () => {});
    ask();
    const timer = setInterval(ask, EVERY);
    return () => {
      controller.abort();
      clearInterval(timer);
    };
  }, [link]);

  if (!latest || !loaded) return null;
  const stale = latest.version !== loaded;
  const line = stale ? `Updated torrential to ${latest.version}.` : latest.notice;
  if (!line || (!stale && line === hidden)) return null;
  const close = () => {
    dismiss(line);
    if (stale) location.reload();
    else setHidden(line);
  };
  return (
    <p role="status" className="flex min-w-0 items-center gap-2 text-sm text-pencil sm:px-3">
      <span className="text-pretty sm:truncate">{line}</span>
      <button
        type="button"
        onClick={close}
        className="shrink-0 rounded-sm text-soft hover:text-ink pointer-coarse:py-2.5"
      >
        {stale ? "Reload" : "Dismiss"}
      </button>
    </p>
  );
}
