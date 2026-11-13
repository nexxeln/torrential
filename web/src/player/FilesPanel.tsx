import { CheckIcon, XMarkIcon } from "@heroicons/react/16/solid";
import { useEffect, useRef } from "react";
import type { TorrentFile } from "../api";
import { bytes } from "../lib/format";
import { fileName, readable } from "../lib/release";
import { position } from "../lib/storage";
import { Link, watchPath } from "../router";

interface Props {
  id: string;
  /** The torrent's videos, in playing order. */
  videos: TorrentFile[];
  current: number;
  onClose: () => void;
}

/** The words every name starts with, such as the show's title before each episode's number. */
function sharedStart(names: string[]): string {
  const words = names.map((name) => name.split(" "));
  let shared = 0;
  while (words.every((own) => own.length > shared + 1 && own[shared] === words[0]![shared])) shared += 1;
  return words[0]!.slice(0, shared).join(" ");
}

function spaced(path: string): string {
  const name = readable(fileName(path));
  return `${name.title} ${name.tags}`.trim();
}

/** The other videos of a torrent, such as the episodes of a season. */
export function FilesPanel({ id, videos, current, onClose }: Props) {
  const playing = useRef<HTMLAnchorElement>(null);
  useEffect(() => playing.current?.focus(), []);
  const shared = sharedStart(videos.map((file) => spaced(file.path)));

  return (
    <aside
      aria-label="Videos in this torrent"
      className="absolute inset-y-0 right-0 z-30 flex w-88 max-w-full animate-appear flex-col bg-raised/95 inset-ring inset-ring-white/10 backdrop-blur-md motion-reduce:animate-none"
    >
      <div className="flex items-center justify-between py-3 pr-3 pl-5">
        <h2 className="truncate text-sm font-medium text-ink">
          {shared} <span className="font-normal text-pencil">{videos.length} videos</span>
        </h2>
        <button
          type="button"
          aria-label="Close"
          onClick={onClose}
          className="relative hit-area flex size-8 items-center justify-center rounded-lg hover:bg-white/10"
        >
          <XMarkIcon className="size-4 fill-soft" />
        </button>
      </div>
      <ul role="list" className="flex-1 overflow-y-auto px-2 pb-2">
        {videos.map((file) => {
          const name = spaced(file.path).slice(shared.length).trim();
          const seen = position(id, file.index);
          return (
            <li key={file.index}>
              <Link
                ref={file.index === current ? playing : undefined}
                href={watchPath(id, file.index)}
                replace
                aria-current={file.index === current ? "true" : undefined}
                className="group/file flex items-start gap-2 rounded-lg px-3 py-2 text-sm hover:bg-white/10 aria-current:bg-white/10 pointer-coarse:py-3"
              >
                <div className="min-w-0 flex-1">
                  <div className="truncate font-medium text-ink">{name}</div>
                  <div className="text-xs text-pencil tabular-nums">
                    {bytes(file.size)}
                    {file.index === current && " · playing"}
                  </div>
                </div>
                {seen?.finished && <CheckIcon aria-label="Watched" className="size-4 h-lh shrink-0 fill-action" />}
              </Link>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}
