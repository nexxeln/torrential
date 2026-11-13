import { XMarkIcon } from "@heroicons/react/16/solid";
import { useState } from "react";
import type { Job } from "../api";
import { bytes, clock, percent, rate } from "../lib/format";
import { useJobs } from "../lib/jobs";
import { fileName, readable } from "../lib/release";
import { forgetPosition, type Position, positions } from "../lib/storage";
import { Link, watchPath } from "../router";
import { Meter } from "../ui/Meter";

const SHOWN = 6;
const ROW = "grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-6 gap-y-1.5 rounded-lg px-3 py-2.5 hover:bg-white/5";

function Unfinished({ entry, onForget }: { entry: Position; onForget: () => void }) {
  const name = readable(fileName(entry.path));
  return (
    <li className="group/row relative">
      <Link href={watchPath(entry.id, entry.file)} className={`${ROW} pr-12`}>
        <div className="flex min-w-0 flex-col gap-1.5">
          <div className="truncate text-base font-medium text-ink sm:text-sm">
            {name.title} <span className="font-normal text-pencil">{name.tags}</span>
          </div>
          <Meter fraction={entry.time / entry.duration} label="Watched" />
        </div>
        <div className="text-sm text-soft tabular-nums">{clock(entry.duration - entry.time)} left</div>
      </Link>
      <button
        type="button"
        aria-label={`Forget where ${name.title} stopped`}
        title="Forget this position"
        onClick={onForget}
        className="absolute top-1/2 right-0 hit-area flex size-7 -translate-y-1/2 items-center justify-center rounded-md hover:bg-white/10"
      >
        <XMarkIcon className="size-4 fill-pencil" />
      </button>
    </li>
  );
}

function summary(job: Job): string {
  const { progress } = job;
  const share = progress.size ? percent(progress.verified / progress.size) : null;
  switch (job.state) {
    case "downloading":
      return [share, rate(progress.down_rate)].filter(Boolean).join(" · ");
    case "done":
      return progress.size ? bytes(progress.size) : "Done";
    case "paused":
      return ["Paused", share].filter(Boolean).join(" · ");
    case "error":
      return "Failed";
    case "metadata":
      return "Finding peers";
    case "queued":
      return "Queued";
    case "checking":
      return "Checking";
    case "interrupted":
      return "Interrupted";
  }
}

function Download({ job }: { job: Job }) {
  const name = readable(job.name ?? job.id.slice(0, 8));
  return (
    <li>
      <Link href={watchPath(job.id)} className={ROW}>
        <div className="truncate text-base font-medium text-ink sm:text-sm">
          {name.title} <span className="font-normal text-pencil">{name.tags}</span>
        </div>
        <div className="text-sm text-soft tabular-nums data-failed:text-error" data-failed={job.state === "error" ? "" : undefined}>
          {summary(job)}
        </div>
      </Link>
    </li>
  );
}

/** What there is to pick up again, shown before a search: unfinished videos first, then the latest downloads. */
export function Recent() {
  const [unfinished, setUnfinished] = useState(() => positions().filter((entry) => !entry.finished));
  const { jobs, link } = useJobs();
  const others = jobs.filter((job) => !unfinished.some((entry) => entry.id === job.id));
  const forget = (entry: Position) => {
    forgetPosition(entry.id, entry.file);
    setUnfinished(positions().filter((kept) => !kept.finished));
  };

  if (unfinished.length === 0 && others.length === 0) {
    return (
      <p className="max-w-[60ch] text-base text-pretty text-pencil sm:text-sm">
        {link === "connecting"
          ? "Connecting to the torrential engine."
          : "Search for something to watch. It plays here while it downloads."}
      </p>
    );
  }
  return (
    <div className="flex flex-col gap-8">
      {unfinished.length > 0 && (
        <section aria-labelledby="continue" className="flex flex-col gap-1">
          <h2 id="continue" className="text-sm font-medium text-pencil">
            Continue watching
          </h2>
          <ul role="list" className="-mx-3">
            {unfinished.slice(0, SHOWN).map((entry) => (
              <Unfinished key={`${entry.id}:${entry.file}`} entry={entry} onForget={() => forget(entry)} />
            ))}
          </ul>
        </section>
      )}
      {others.length > 0 && (
        <section aria-labelledby="downloads" className="flex flex-col gap-1">
          <div className="flex items-baseline justify-between">
            <h2 id="downloads" className="text-sm font-medium text-pencil">
              Downloads
            </h2>
            {others.length > SHOWN && (
              <Link href="/downloads" className="rounded-sm text-sm text-soft hover:text-ink">
                All {jobs.length}
              </Link>
            )}
          </div>
          <ul role="list" className="-mx-3">
            {others.slice(0, SHOWN).map((job) => (
              <Download key={job.id} job={job} />
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
