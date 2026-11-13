import { isActive, useJobs } from "../lib/jobs";
import { useOnMac } from "../lib/mac";
import { Link } from "../router";

/** `current` is the page of the main navigation this one is, if any. */
export function Header({ current }: { current: "search" | "downloads" | "devices" | null }) {
  const { jobs, link } = useJobs();
  const onMac = useOnMac();
  const active = jobs.filter(isActive).length;
  const item = "rounded-lg px-2.5 py-1.5 text-sm font-medium text-soft hover:bg-white/5 hover:text-ink aria-[current=page]:text-ink";
  return (
    <header className="flex items-center justify-between gap-2 py-4">
      <Link href="/" className="rounded-lg text-base font-semibold tracking-tight text-ink">
        torrential
      </Link>
      {link === "lost" && (
        <p role="status" className="truncate px-3 text-sm text-error">
          The engine is not running. Start it with <code className="font-mono">torrential web</code>.
        </p>
      )}
      <nav aria-label="Main" className="-mr-2.5 flex shrink-0 items-center gap-1">
        <Link href="/" aria-current={current === "search" ? "page" : undefined} className={item}>
          Search
        </Link>
        <Link href="/downloads" aria-current={current === "downloads" ? "page" : undefined} className={item}>
          Downloads
          {active > 0 && <span className="text-action tabular-nums"> {active}</span>}
        </Link>
        {onMac && (
          <Link href="/devices" aria-current={current === "devices" ? "page" : undefined} className={item}>
            Devices
          </Link>
        )}
      </nav>
    </header>
  );
}
