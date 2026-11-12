import { BackwardIcon, ForwardIcon } from "@heroicons/react/20/solid";
import { PauseIcon, PlayIcon } from "@heroicons/react/24/solid";
import type { CSSProperties } from "react";
import { usePlayer } from "./context";
import { useFeedback } from "./feedback";

/** What the last action did, drawn over the picture for a moment: a symbol, a jump, or a line of text. */
export function FeedbackLayer() {
  const { bezel, seek, note } = useFeedback(usePlayer().feedback);
  const back = seek !== null && seek.seconds < 0;
  const Bezel = bezel?.icon === "play" ? PlayIcon : PauseIcon;
  const Jump = back ? BackwardIcon : ForwardIcon;

  return (
    <div className="pointer-events-none absolute inset-0">
      {bezel && (
        <div
          key={bezel.at}
          className="absolute top-1/2 left-1/2 -mt-10 -ml-10 flex size-20 animate-bezel items-center justify-center rounded-full bg-black/55 motion-reduce:animate-none"
        >
          <Bezel className="size-6 fill-white" />
        </div>
      )}
      {seek && (
        <div
          key={`${back}`}
          data-side={back ? "left" : "right"}
          className="absolute top-1/2 flex -translate-y-1/2 animate-pop items-center gap-2 rounded-full bg-black/55 py-2.5 pr-4 pl-3 text-base font-semibold text-white tabular-nums data-[side=left]:left-[12%] data-[side=right]:right-[12%] motion-reduce:animate-none"
        >
          <Jump className="size-5 shrink-0 fill-white" />
          {Math.abs(seek.seconds)} s
        </div>
      )}
      <div role="status" aria-live="polite" className="absolute inset-x-0 top-[9%] flex justify-center px-6">
        {note && (
          <div className="flex animate-pop flex-col items-center gap-2 rounded-xl bg-black/65 px-4 py-2.5 text-base font-medium text-white tabular-nums motion-reduce:animate-none">
            {note.text}
            {note.level !== undefined && (
              <div className="h-1 w-36 overflow-hidden rounded-full bg-white/25">
                <div className="h-full w-(--level) rounded-full bg-white" style={{ "--level": `${note.level * 100}%` } as CSSProperties} />
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
