import { ArrowUpIcon, ClipboardDocumentCheckIcon, ClipboardDocumentIcon } from "@heroicons/react/16/solid";
import { useEffect, useRef, useState } from "react";
import type { Flag, NameCodec, Release, Resolution } from "../api";
import { age, bytes } from "../lib/format";
import { isVideo, readable } from "../lib/release";
import { isTv } from "../lib/tv";
import { Link, watchPath } from "../router";
import { Chip } from "../ui/Chip";

export const RESOLUTIONS: Record<Resolution, string> = { sd: "SD", "720p": "720p", "1080p": "1080p", "2160p": "4K" };
export const CODECS: Record<NameCodec, string> = { h264: "H.264", hevc: "HEVC", av1: "AV1" };
const FLAGS: Record<Flag, { label: string; tone: "good" | "bad" | "plain"; title: string }> = {
  official: { label: "Official", tone: "good", title: "Published by the project that made it" },
  trusted: { label: "Trusted", tone: "good", title: "The index marks the uploader as trusted" },
  public_domain: { label: "Public domain", tone: "good", title: "The source lists a public-domain license" },
  creative_commons: { label: "CC", tone: "good", title: "The source lists a Creative Commons license" },
  cam: { label: "Cam", tone: "bad", title: "A camera recording, usually low quality" },
  adult: { label: "Adult", tone: "plain", title: "Adult content" },
};

/** The command that downloads a release the player cannot show. */
export function getCommand(release: Release): string {
  return `torrential get ${release.id.slice(0, 8)}`;
}

interface Props {
  release: Release;
  selected: boolean;
  /** The row was copied by keyboard a moment ago. */
  copied: boolean;
  onPoint: () => void;
  onCopy: () => void;
  /** Where a video release plays. Its own download by default. */
  href?: string;
  /** The release a title's Play tries first. */
  pick?: boolean;
  /** Why the release did not start when Play last tried it, as a sentence. */
  failed?: string;
}

const ROW =
  "grid w-full grid-cols-1 items-center gap-x-6 gap-y-1.5 rounded-lg px-3 py-2.5 text-left hover:bg-white/5 data-selected:bg-white/8 sm:grid-cols-[minmax(0,1fr)_auto]";

export function ResultRow({ release, selected, copied, onPoint, onCopy, href, pick, failed }: Props) {
  const row = useRef<HTMLLIElement>(null);
  useEffect(() => {
    if (selected) row.current?.scrollIntoView({ block: "nearest" });
  }, [selected]);

  const name = readable(release.name);
  const { quality } = release;
  const playable = isVideo(release);
  const body = (
    <>
      <div className="flex min-w-0 flex-col gap-1">
        <div className="truncate text-base font-medium text-ink sm:text-sm" title={release.name}>
          {name.title} <span className="font-normal text-pencil">{name.tags}</span>
        </div>
        <div className="flex flex-wrap items-center gap-1.5 text-sm text-pencil">
          {pick && (
            <Chip tone="good" title="Play tries this source first">
              Pick
            </Chip>
          )}
          {failed && <Chip title={failed}>Did not start earlier</Chip>}
          {quality.resolution && <Chip>{RESOLUTIONS[quality.resolution]}</Chip>}
          {quality.codec && <Chip>{CODECS[quality.codec]}</Chip>}
          {quality.hdr && <Chip>HDR</Chip>}
          {release.flags.map((flag) => (
            <Chip key={flag} tone={FLAGS[flag].tone} title={FLAGS[flag].title}>
              {FLAGS[flag].label}
            </Chip>
          ))}
          {!playable && <Chip>{release.category ?? "other"}</Chip>}
          <span className="truncate pl-0.5">{release.sources.join(", ")}</span>
        </div>
      </div>
      <div className="flex items-center gap-4 text-sm text-soft tabular-nums sm:justify-end">
        <div className="sm:w-20 sm:text-right" title="Size">
          {release.size === null ? "–" : bytes(release.size)}
        </div>
        <div
          className="flex items-center gap-0.5 data-dead:text-error sm:w-16 sm:justify-end"
          data-dead={release.seeders === 0 ? "" : undefined}
          title="Seeders: peers that have the whole torrent"
        >
          <ArrowUpIcon className="size-4 shrink-0 fill-pencil" />
          {release.seeders === null ? "–" : release.seeders.toLocaleString("en")}
        </div>
        <div className="text-pencil sm:w-9 sm:text-right" title="Published">
          {release.published === null ? "–" : age(release.published)}
        </div>
        {!playable && <CopyState copied={copied} />}
      </div>
    </>
  );

  return (
    <li ref={row} className="scroll-my-24">
      {playable ? (
        <Link
          href={href ?? watchPath(release.id)}
          data-selected={selected ? "" : undefined}
          aria-current={selected ? "true" : undefined}
          onPointerMove={onPoint}
          onFocus={isTv() ? onPoint : undefined}
          className={ROW}
        >
          {body}
        </Link>
      ) : (
        <button
          type="button"
          data-selected={selected ? "" : undefined}
          aria-current={selected ? "true" : undefined}
          aria-label={`Copy the command that downloads ${release.name}`}
          title={`Not a video. Copy "${getCommand(release)}" to download it from a terminal.`}
          onPointerMove={onPoint}
          onFocus={isTv() ? onPoint : undefined}
          onClick={onCopy}
          className={ROW}
        >
          {body}
        </button>
      )}
    </li>
  );
}

function CopyState({ copied }: { copied: boolean }) {
  const Icon = copied ? ClipboardDocumentCheckIcon : ClipboardDocumentIcon;
  return <Icon aria-hidden className={`size-4 shrink-0 ${copied ? "fill-action" : "fill-pencil"}`} />;
}

/** Tracks which release's command was copied a moment ago. */
export function useCopied(): [string | null, (release: Release) => void] {
  const [copied, setCopied] = useState<string | null>(null);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(null), 2000);
    return () => clearTimeout(timer);
  }, [copied]);
  const copy = (release: Release) => {
    void navigator.clipboard?.writeText(getCommand(release)).catch(() => {});
    setCopied(release.id);
  };
  return [copied, copy];
}
