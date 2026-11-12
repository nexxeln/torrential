import { type CSSProperties, type PointerEvent, useMemo, useRef, useState } from "react";
import type { Chapter, PlayInfo } from "../api";
import { clock } from "../lib/format";
import { usePlayingJob } from "./Chrome";
import { usePlayer } from "./context";
import { type Downloaded, isDownloaded, useDownloaded } from "./downloaded";
import { useFrame } from "./frame";
import { useThumbnail } from "./thumbnails";

interface Props {
  play: PlayInfo;
  /** Null while the video's length is unknown, which disables the bar. */
  duration: number | null;
  chapters: Chapter[];
  /** The engine can make preview frames of this file. */
  frames: boolean;
  /** Width over height of the picture, for the preview's shape. */
  aspect: number;
  onSeek: (time: number) => void;
}

type Range = [from: number, to: number, color: string];

/** A horizontal gradient that paints each range, in percent, and nothing between them. */
function paint(ranges: Range[]): string {
  if (ranges.length === 0) return "none";
  const stops: string[] = [];
  let cursor = 0;
  for (const [from, to, color] of ranges) {
    stops.push(`transparent ${cursor}% ${from}%`, `${color} ${from}% ${to}%`);
    cursor = to;
  }
  stops.push(`transparent ${cursor}% 100%`);
  return `linear-gradient(to right, ${stops.join(", ")})`;
}

function downloadedPaint(downloaded: Downloaded): string {
  const ranges: Range[] = [];
  const run = 100 / downloaded.length;
  downloaded.forEach((share, i) => {
    if (share <= 0) return;
    const color = `rgb(255 255 255 / ${(0.1 + share * 0.22).toFixed(2)})`;
    const last = ranges.at(-1);
    if (last && last[2] === color && Math.abs(last[1] - i * run) < 0.001) last[1] = (i + 1) * run;
    else ranges.push([i * run, (i + 1) * run, color]);
  });
  return paint(ranges);
}

/** A mask that cuts a thin gap into the bar where each chapter after the first begins. */
function chapterMask(chapters: Chapter[], duration: number): string | undefined {
  const cuts = chapters.map((chapter) => (chapter.start / duration) * 100).filter((at) => at > 0.2 && at < 99.8);
  if (cuts.length === 0) return undefined;
  const stops = cuts.flatMap((at) => [
    `#000 calc(${at}% - 1.5px)`,
    `transparent calc(${at}% - 1.5px)`,
    `transparent calc(${at}% + 1.5px)`,
    `#000 calc(${at}% + 1.5px)`,
  ]);
  return `linear-gradient(to right, #000 0, ${stops.join(", ")}, #000 100%)`;
}

const clamp = (value: number) => Math.min(1, Math.max(0, value));
const at = (fraction: number) => ({ "--at": `${fraction * 100}%` }) as CSSProperties;

/**
 * The timeline. From dim to bright it shows what has not downloaded, what the torrent has on disk,
 * what the browser has buffered, and what has played. The playhead is drawn every frame straight
 * to the DOM, so playing renders nothing in React.
 */
export function SeekBar({ play, duration, chapters, frames, aspect, onSeek }: Props) {
  const { element, scrub } = usePlayer();
  const downloaded = useDownloaded(usePlayingJob(play), play.video, play.total_size);
  const bar = useRef<HTMLDivElement>(null);
  const played = useRef<HTMLDivElement>(null);
  const thumb = useRef<HTMLDivElement>(null);
  const buffered = useRef<HTMLDivElement>(null);
  const drawn = useRef({ fraction: -1, buffered: "", second: -1 });
  /** Where the pointer is along the bar, from 0 to 1. */
  const [pointed, setPointed] = useState<number | null>(null);
  const [dragging, setDragging] = useState(false);
  const length = duration ?? 0;

  useFrame(() => {
    if (!length || !bar.current) return;
    const time = scrub.value ?? element.currentTime;
    const fraction = clamp(time / length);
    if (Math.abs(fraction - drawn.current.fraction) > 0.00002) {
      drawn.current.fraction = fraction;
      played.current!.style.scale = `${fraction} 1`;
      thumb.current!.style.left = `${fraction * 100}%`;
    }
    const ranges: Range[] = [];
    for (let i = 0; i < element.buffered.length; i++) {
      const from = clamp(element.buffered.start(i) / length) * 100;
      const to = clamp(element.buffered.end(i) / length) * 100;
      ranges.push([Math.round(from * 20) / 20, Math.round(to * 20) / 20, "rgb(255 255 255 / 0.5)"]);
    }
    const signature = ranges.join(";");
    if (signature !== drawn.current.buffered) {
      drawn.current.buffered = signature;
      buffered.current!.style.background = paint(ranges);
    }
    const second = Math.floor(time);
    if (second !== drawn.current.second) {
      drawn.current.second = second;
      bar.current.setAttribute("aria-valuenow", String(second));
      bar.current.setAttribute("aria-valuetext", `${clock(time)} of ${clock(length)}`);
    }
  });

  const under = (event: PointerEvent): number => {
    const box = bar.current!.getBoundingClientRect();
    return clamp((event.clientX - box.left) / box.width);
  };
  const down = (event: PointerEvent<HTMLDivElement>) => {
    if (!length || event.button !== 0) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    setDragging(true);
    setPointed(under(event));
    scrub.set(under(event) * length);
  };
  const move = (event: PointerEvent<HTMLDivElement>) => {
    setPointed(under(event));
    if (dragging) scrub.set(under(event) * length);
  };
  const up = (event: PointerEvent<HTMLDivElement>) => {
    if (!dragging) return;
    setDragging(false);
    scrub.set(null);
    onSeek(under(event) * length);
    if (event.pointerType !== "mouse") setPointed(null);
  };
  const cancel = () => {
    setDragging(false);
    scrub.set(null);
    setPointed(null);
  };

  const downloadedLayer = useMemo(() => downloadedPaint(downloaded), [downloaded]);
  const mask = useMemo(() => (length ? chapterMask(chapters, length) : undefined), [chapters, length]);
  const time = pointed === null ? null : pointed * length;
  const chapter = time === null ? null : (chapters.find((c) => c.start <= time && time < c.end) ?? null);
  // The engine draws the frame at the ten seconds below, and only where the file around it is on disk.
  const frameAt = time === null || length <= 0 ? null : (Math.floor(time / 10) * 10) / length;
  const framed = frames && frameAt !== null && isDownloaded(downloaded, frameAt, frameMargin(play.video.size));

  return (
    <div
      ref={bar}
      role="slider"
      tabIndex={0}
      aria-label="Seek"
      aria-orientation="horizontal"
      aria-valuemin={0}
      aria-valuemax={Math.round(length)}
      aria-disabled={!length}
      data-dragging={dragging ? "" : undefined}
      className="group/seek relative h-6 cursor-pointer touch-none rounded-sm aria-disabled:cursor-default"
      onPointerDown={down}
      onPointerMove={move}
      onPointerUp={up}
      onPointerCancel={cancel}
      onPointerLeave={() => !dragging && setPointed(null)}
    >
      <div
        className="absolute inset-x-0 top-1/2 h-1 -translate-y-1/2 overflow-hidden rounded-full bg-black/35 transition-[height] duration-150 group-hover/seek:h-1.75 group-focus-visible/seek:h-1.75 group-data-dragging/seek:h-1.75 motion-reduce:transition-none"
        style={{ maskImage: mask, WebkitMaskImage: mask }}
      >
        <div className="absolute inset-0 bg-white/15" />
        <div className="absolute inset-0" style={{ background: downloadedLayer }} />
        <div ref={buffered} className="absolute inset-0" />
        {pointed !== null && <div className="absolute inset-y-0 left-0 w-(--at) bg-white/25" style={at(pointed)} />}
        <div ref={played} className="absolute inset-0 origin-left scale-x-0 bg-action" />
      </div>
      <div
        ref={thumb}
        className="pointer-events-none absolute top-1/2 size-3.5 -translate-1/2 scale-0 rounded-full bg-ink ring-1 ring-black/20 transition-transform duration-150 group-hover/seek:scale-100 group-focus-visible/seek:scale-100 group-data-dragging/seek:scale-110 motion-reduce:transition-none"
      />
      {pointed !== null && time !== null && length > 0 && (
        <Preview
          at={pointed}
          time={time}
          chapter={chapter?.title ?? null}
          id={play.id}
          file={play.video.index}
          framed={framed}
          aspect={aspect}
        />
      )}
    </div>
  );
}

interface PreviewProps {
  /** Along the bar, from 0 to 1. */
  at: number;
  time: number;
  chapter: string | null;
  id: string;
  file: number;
  /** This part of the file has downloaded, so a frame of it can be shown. */
  framed: boolean;
  aspect: number;
}

/**
 * How much of the file must be on disk each side of a frame, as a share of it. The engine asks for
 * 8 MiB or 2% of the file, whichever is more, and this adds a run of the map for its rounding.
 */
function frameMargin(size: number): number {
  return Math.max((8 * 1024 * 1024) / size, 0.02) + 0.005;
}

/** What is under the pointer: the time, the chapter, and the picture where the file has it. */
function Preview({ at: fraction, time, chapter, id, file, framed, aspect }: PreviewProps) {
  const frame = useThumbnail(id, file, framed ? time : null);
  // The last picture stays while the next one loads, so moving along the bar does not flicker.
  const held = useRef<string | null>(null);
  if (frame) held.current = frame;
  if (!framed) held.current = null;
  const picture = frame ?? held.current;

  return (
    <div
      className="pointer-events-none absolute bottom-full left-[clamp(var(--half),var(--at),100%-var(--half))] flex -translate-x-1/2 flex-col items-center gap-1.5 pb-1 [--half:--spacing(26)]"
      style={at(fraction)}
    >
      {picture && (
        <img
          src={picture}
          alt=""
          className="w-52 rounded-lg bg-black object-cover ring-1 ring-white/25"
          style={{ aspectRatio: aspect }}
        />
      )}
      <div className="flex max-w-52 flex-col items-center text-center text-over-video">
        {chapter && <div className="max-w-full truncate text-xs font-medium text-ink/85">{chapter}</div>}
        <div className="text-sm font-semibold text-ink tabular-nums">{clock(time)}</div>
      </div>
    </div>
  );
}
