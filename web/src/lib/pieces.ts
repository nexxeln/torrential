import type { Job, StreamedFile } from "../api";

/** A run of a file, as fractions of its length, and the share of it that has downloaded. */
export interface Arrived {
  from: number;
  to: number;
  /** 0 to 1. */
  share: number;
}

const WHOLE: Arrived[] = [{ from: 0, to: 1, share: 1 }];

/**
 * Which parts of `file` are on disk, read from the job's piece map. The map covers the whole
 * torrent in 64 equal runs, so a small file inside a large torrent reads coarsely.
 */
export function arrived(job: Job, file: StreamedFile, totalSize: number): Arrived[] {
  if (job.state === "done") return WHOLE;
  const map = job.progress.piece_map;
  if (!map || totalSize <= 0 || file.size <= 0) return [];
  const run = totalSize / map.length;
  const runs: Arrived[] = [];
  for (let i = 0; i < map.length; i++) {
    const from = Math.max(0, (i * run - file.offset) / file.size);
    const to = Math.min(1, ((i + 1) * run - file.offset) / file.size);
    if (to <= 0 || from >= 1) continue;
    const eighths = Number(map[i]);
    const share = Number.isNaN(eighths) ? 0 : eighths / 8;
    const last = runs.at(-1);
    if (last && last.share === share) last.to = to;
    else runs.push({ from, to, share });
  }
  return runs.filter((r) => r.share > 0);
}
