import { useEffect, useState } from "react";
import { type ApiError, asApiError, change, type Job } from "../api";
import { bytes, count, percent, rate, sentence, span } from "../lib/format";
import { useJobs } from "../lib/jobs";
import { readable } from "../lib/release";
import { Link, watchPath } from "../router";
import { Button, buttonClass } from "../ui/Button";
import { Header } from "../ui/Header";
import { Meter } from "../ui/Meter";

const STATES: Record<Job["state"], string> = {
  queued: "Queued",
  metadata: "Finding peers",
  checking: "Checking files",
  downloading: "Downloading",
  paused: "Paused",
  done: "Done",
  error: "Failed",
};

function facts(job: Job): string {
  const { progress } = job;
  const size = progress.size;
  const have = size === null ? null : job.state === "done" ? bytes(size) : `${bytes(progress.verified)} of ${bytes(size)}`;
  const moving = job.state === "downloading";
  return [
    STATES[job.state],
    have,
    moving ? rate(progress.down_rate) : null,
    moving ? count(progress.peers, "peer") : null,
    moving && progress.eta !== null ? `${span(progress.eta)} left` : null,
  ]
    .filter(Boolean)
    .join(" · ");
}

type Action = "pause" | "resume" | "remove";

function Row({ job, onFailed }: { job: Job; onFailed: (error: ApiError) => void }) {
  const [busy, setBusy] = useState<Action | null>(null);
  const name = readable(job.name ?? job.id.slice(0, 8));
  const { progress } = job;
  const fraction = job.state === "done" ? 1 : progress.size ? progress.verified / progress.size : 0;
  const stopped = job.state === "paused" || job.state === "error";
  const act = (action: Action) => {
    setBusy(action);
    change(action, [job.id])
      .catch((thrown) => onFailed(asApiError(thrown)))
      .finally(() => setBusy(null));
  };

  return (
    <li className="grid grid-cols-1 items-center gap-x-6 gap-y-2 py-4 sm:grid-cols-[minmax(0,1fr)_--spacing(60)]">
      <div className="flex min-w-0 flex-col gap-2">
        <div className="truncate text-base font-medium text-ink sm:text-sm" title={job.name ?? undefined}>
          {name.title} <span className="font-normal text-pencil">{name.tags}</span>
        </div>
        <Meter fraction={fraction} label="Downloaded" tone={job.state === "downloading" || job.state === "done" ? "action" : "muted"} />
        <div className="flex flex-col gap-0.5 text-sm tabular-nums">
          <p className="truncate text-pencil">
            {facts(job)}
            {job.state !== "done" && progress.size ? ` · ${percent(fraction)}` : ""}
          </p>
          {job.error && (
            <p className="text-pretty text-error">
              {sentence(job.error.message)}
              {job.error.hint && <span className="text-pencil"> {sentence(job.error.hint)}</span>}
            </p>
          )}
        </div>
      </div>
      <div className="flex items-center gap-1 sm:justify-end">
        <Link href={watchPath(job.id)} className={buttonClass({})}>
          Watch
        </Link>
        {job.state !== "done" &&
          (stopped ? (
            <Button variant="quiet" disabled={busy !== null} onClick={() => act("resume")}>
              {job.state === "error" ? "Retry" : "Resume"}
            </Button>
          ) : (
            <Button variant="quiet" disabled={busy !== null} onClick={() => act("pause")}>
              Pause
            </Button>
          ))}
        <Button
          variant="quiet"
          disabled={busy !== null}
          title="Takes it off this list. The files stay on disk."
          onClick={() => act("remove")}
        >
          Remove
        </Button>
      </div>
    </li>
  );
}

export function DownloadsPage() {
  const { jobs, link } = useJobs();
  const [problem, setProblem] = useState<ApiError | null>(null);
  useEffect(() => {
    document.title = "Downloads · torrential";
  }, []);

  return (
    <div className="mx-auto flex min-h-dvh max-w-4xl flex-col px-page pb-16">
      <Header current="downloads" />
      <main className="flex flex-col gap-2 pt-[min(8vh,--spacing(16))]">
        <div className="flex items-baseline justify-between gap-4">
          <h1 className="text-lg font-medium text-ink">Downloads</h1>
          <p className="text-sm text-pencil">Removing a download keeps its files.</p>
        </div>
        {problem && (
          <p role="alert" className="text-sm text-pretty text-error">
            {sentence(problem.message)}
            {problem.hint && <span className="text-pencil"> {sentence(problem.hint)}</span>}
          </p>
        )}
        {jobs.length > 0 ? (
          <ul role="list" className="divide-y divide-white/8">
            {jobs.map((job) => (
              <Row key={job.id} job={job} onFailed={setProblem} />
            ))}
          </ul>
        ) : (
          <p className="max-w-[60ch] pt-4 text-base text-pretty text-pencil sm:text-sm">
            {link === "connecting" ? "Connecting to the torrential engine." : "Nothing has been downloaded yet. What you watch shows up here."}
          </p>
        )}
      </main>
    </div>
  );
}
