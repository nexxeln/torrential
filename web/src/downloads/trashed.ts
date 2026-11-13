import type { Change } from "../api";
import { count } from "../lib/format";

/** What "Delete files" did with the files of `name`, in a sentence. */
export function trashed(name: string, change: Change): string {
  const moved = change.trashed?.length ?? 0;
  const kept = change.kept ?? [];
  const why = kept[0]?.reason;
  if (moved > 0 && why) return `The files of ${name} are in the Trash, except ${count(kept.length, "file")} that stayed, for ${why}.`;
  if (moved > 0) return `The files of ${name} are in the Trash.`;
  if (why) return `The files of ${name} stayed, for ${why}.`;
  return `${name} had no files on disk.`;
}
