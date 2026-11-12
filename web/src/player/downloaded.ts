import { useEffect, useMemo, useState } from "react";
import { type Job, pieces, type StreamedFile } from "../api";
import { arrived } from "../lib/pieces";

/** How much of each equal run of the playing file is on disk, from 0 to 1, in file order. */
export type Downloaded = number[];

const RUNS = 200;
const POLL_MS = 1000;
const GIVE_UP = 3;
const WHOLE: Downloaded = Array.from({ length: RUNS }, () => 1);

/** The torrent-wide map, read for this file: coarse for a small file in a large torrent, but always there. */
function fromTorrentMap(job: Job, file: StreamedFile, totalSize: number): Downloaded {
  const runs = arrived(job, file, totalSize);
  return Array.from({ length: RUNS }, (_, i) => {
    const middle = (i + 0.5) / RUNS;
    return runs.find((run) => run.from <= middle && middle < run.to)?.share ?? 0;
  });
}

/**
 * Which parts of the playing file have downloaded. Asks the engine for the file's own piece map
 * about once a second while the download runs, and reads the job's torrent-wide map when that
 * route does not answer.
 */
export function useDownloaded(job: Job, file: StreamedFile, totalSize: number): Downloaded {
  const [own, setOwn] = useState<Downloaded | null>(null);
  const done = job.state === "done";
  const { id } = job;

  useEffect(() => {
    if (done) return;
    const stop = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    let failures = 0;
    const poll = () =>
      pieces(id, file.index, stop.signal)
        .then((map) => {
          failures = 0;
          setOwn([...map.pieces].map((eighths) => (Number(eighths) || 0) / 8));
        })
        .catch(() => {
          failures += 1;
          setOwn(null);
        })
        .finally(() => {
          // An engine without the route keeps the torrent-wide map, and is not asked forever.
          if (!stop.signal.aborted && failures < GIVE_UP) timer = setTimeout(poll, POLL_MS);
        });
    void poll();
    return () => {
      stop.abort();
      clearTimeout(timer);
    };
  }, [id, file.index, done]);

  const coarse = useMemo(() => (own || done ? null : fromTorrentMap(job, file, totalSize)), [own, done, job, file, totalSize]);
  return done ? WHOLE : (own ?? coarse ?? WHOLE);
}

/**
 * The bytes within `margin` of `fraction` of the file are all on disk. Both are shares of the
 * file's length.
 */
export function isDownloaded(downloaded: Downloaded, fraction: number, margin: number): boolean {
  const run = (share: number) => Math.min(downloaded.length - 1, Math.max(0, Math.floor(share * downloaded.length)));
  for (let at = run(fraction - margin); at <= run(fraction + margin); at++) {
    if ((downloaded[at] ?? 1) < 1) return false;
  }
  return true;
}
