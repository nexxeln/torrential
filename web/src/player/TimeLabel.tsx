import { useRef, useState } from "react";
import type { Chapter } from "../api";
import { clock } from "../lib/format";
import { preferences, savePreferences } from "../lib/storage";
import { usePlayer } from "./context";
import { useFrame } from "./frame";

/** The time played, or on a click the time left, beside the video's length. Written to the DOM each frame. */
export function TimeLabel({ duration }: { duration: number | null }) {
  const { element, scrub } = usePlayer();
  const text = useRef<HTMLSpanElement>(null);
  const [remaining, setRemaining] = useState(() => preferences().showRemaining);

  useFrame(() => {
    if (!text.current) return;
    const time = scrub.value ?? element.currentTime;
    const shown = remaining && duration ? `−${clock(Math.max(0, duration - time))}` : clock(time);
    if (text.current.textContent !== shown) text.current.textContent = shown;
  });

  return (
    <button
      type="button"
      aria-label={remaining ? "Show time played" : "Show time left"}
      title={remaining ? "Show time played" : "Show time left"}
      data-time=""
      onClick={() => {
        savePreferences({ showRemaining: !remaining });
        setRemaining(!remaining);
      }}
      className="shrink-0 rounded-lg px-2 py-1.5 text-sm font-medium whitespace-nowrap text-ink tabular-nums hover:bg-white/15 pointer-coarse:min-h-11"
    >
      <span ref={text}>0:00</span>
      <span className="font-normal text-ink/60 max-sm:hidden"> / {duration ? clock(duration) : "–:––"}</span>
    </button>
  );
}

/** The chapter the playhead is in. Renders only when the playhead crosses into another one. */
export function useChapter(chapters: Chapter[]): Chapter | null {
  const { element } = usePlayer();
  const [index, setIndex] = useState(-1);
  useFrame(() => {
    if (chapters.length === 0) return;
    const time = element.currentTime;
    const now = chapters.findIndex((chapter) => chapter.start <= time && time < chapter.end);
    if (now !== index) setIndex(now);
  });
  return chapters[index] ?? null;
}
