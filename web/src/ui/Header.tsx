import { isActive, useJobs } from "../lib/jobs";
import { Link } from "../router";

/** `current` is the page of the main navigation this one is, if any. */
export function Header({ current }: { current: "search" | "downloads" | null }) {
  const { jobs, link } = useJobs();
  const active = jobs.filter(isActive).length;
  const item =
    "rounded-lg px-2.5 py-1.5 text-sm font-medium text-soft hover:bg-white/5 hover:text-ink aria-[current=page]:text-ink pointer-coarse:py-3";
  return (
    <header className="flex flex-wrap items-center justify-between gap-x-2 py-4 pointer-coarse:py-2">
      <Link href="/" className="rounded-lg text-base font-semibold tracking-tight text-ink pointer-coarse:py-2.5">
        torrential
      </Link>
      {/* A line about the engine. On a phone it takes a row of its own under the navigation. */}
      <div className="min-w-0 max-sm:order-last max-sm:basis-full sm:flex-1">
        {link === "lost" && (
          <p role="status" className="text-sm text-pretty text-error sm:truncate sm:px-3">
            The engine is not running. Start it with <code className="font-mono">torrential web</code>.
          </p>
        )}
      </div>
      <nav aria-label="Main" className="-mr-2.5 flex shrink-0 items-center gap-1">
        <Link href="/" aria-current={current === "search" ? "page" : undefined} className={item}>
          Search
        </Link>
        <Link href="/downloads" aria-current={current === "downloads" ? "page" : undefined} className={item}>
          Downloads
          {active > 0 && <span className="text-action tabular-nums"> {active}</span>}
        </Link>
      </nav>
    </header>
  );
}
