import { isActive, useJobs } from "../lib/jobs";
import { useOnMac } from "../lib/mac";
import { Link } from "../router";

/** The engine listens on loopback for the Mac it runs on. A phone or TV reaches it by the Mac's address on the home network. */
const ON_MAC = ["127.0.0.1", "localhost", "[::1]"].includes(location.hostname);

/** `current` is the page of the main navigation this one is, if any. */
export function Header({ current }: { current: "search" | "downloads" | "devices" | null }) {
  const { jobs, link } = useJobs();
  const onMac = useOnMac();
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
            {ON_MAC ? (
              <>
                The engine is not running. Start it with <code className="font-mono">torrential web</code>.
              </>
            ) : (
              "The Mac’s torrential cannot be reached. Check that the Mac is awake and on this network."
            )}
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
        {onMac && (
          <Link href="/devices" aria-current={current === "devices" ? "page" : undefined} className={item}>
            Devices
          </Link>
        )}
      </nav>
    </header>
  );
}
