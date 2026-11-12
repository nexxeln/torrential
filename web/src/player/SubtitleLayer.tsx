import { type CSSProperties, useState } from "react";
import type { SubtitleStyle } from "../lib/storage";
import { usePlayer } from "./context";
import type { CueTrack } from "./cues";
import { useFrame } from "./frame";
import type { Cue, Span } from "./vtt";

const BACKING = {
  shadow: "text-over-video",
  outline: "text-outlined",
  box: "",
};

/** Shown while the style is being changed and no line is on screen, so there is something to judge by. */
const SAMPLE: Cue = {
  start: 0,
  end: 0,
  place: "bottom",
  text: "sample",
  lines: [[{ text: "Subtitles look like this.", italic: false, bold: false, underline: false }]],
};

function Run({ span }: { span: Span }) {
  const style = [span.italic && "italic", span.bold && "font-bold", span.underline && "underline"].filter(Boolean).join(" ");
  return style ? <span className={style}>{span.text}</span> : span.text;
}

function Lines({ cue, boxed }: { cue: Cue; boxed: boolean }) {
  return cue.lines.map((line, row) => (
    <p key={row}>
      <span className={boxed ? "rounded-[0.18em] bg-black/75 box-decoration-clone px-[0.35em] py-[0.08em]" : undefined}>
        {line.map((span, run) => (
          <Run key={run} span={span} />
        ))}
      </span>
    </p>
  ));
}

interface Props {
  track: CueTrack | null;
  /** Seconds the lines show later than written. Negative shows them earlier. */
  offset: number;
  style: SubtitleStyle;
  /** The controls are showing, so the lines move up clear of them. */
  lifted: boolean;
  /** Show a sample line when nothing else is on screen. */
  sample: boolean;
}

/** The subtitle lines over the picture. Follows the playhead every frame and renders when the lines change. */
export function SubtitleLayer({ track, offset, style, lifted, sample }: Props) {
  const { element } = usePlayer();
  const [cues, setCues] = useState<Cue[]>([]);
  const shown = cues.map((cue) => `${cue.start}:${cue.text}`).join("|");

  useFrame(() => {
    const active = track ? track.at(element.currentTime - offset) : [];
    if (active.map((cue) => `${cue.start}:${cue.text}`).join("|") !== shown) setCues(active);
  });

  const visible = cues.length > 0 ? cues : sample ? [SAMPLE] : [];
  if (visible.length === 0) return null;
  const boxed = style.backing === "box";
  const groups = (["top", "middle", "bottom"] as const).map((place) => visible.filter((cue) => cue.place === place));
  const [top, middle, bottom] = groups as [Cue[], Cue[], Cue[]];
  const stack = `absolute inset-x-[6%] flex flex-col items-center gap-[0.15em] text-center ${BACKING[style.backing]}`;

  return (
    <div
      data-lifted={lifted ? "" : undefined}
      data-subtitles=""
      className="group/subtitles pointer-events-none absolute inset-0 text-[calc(clamp(1.125rem,min(2.6vw,4.6vh),3.25rem)*var(--scale))]/[1.3] font-medium text-white"
      style={{ "--scale": style.scale, "--lift": `${style.lift}%` } as CSSProperties}
    >
      {top.length > 0 && (
        <div className={`${stack} top-[5%] transition-[top] duration-200 group-data-lifted/subtitles:top-20 motion-reduce:transition-none`}>
          {top.map((cue) => (
            <Lines key={`${cue.start}:${cue.text}`} cue={cue} boxed={boxed} />
          ))}
        </div>
      )}
      {middle.length > 0 && (
        <div className={`${stack} top-1/2 -translate-y-1/2`}>
          {middle.map((cue) => (
            <Lines key={`${cue.start}:${cue.text}`} cue={cue} boxed={boxed} />
          ))}
        </div>
      )}
      {bottom.length > 0 && (
        <div
          className={`${stack} bottom-(--lift) transition-[bottom] duration-200 group-data-lifted/subtitles:bottom-[max(var(--lift),--spacing(30))] motion-reduce:transition-none`}
        >
          {bottom.map((cue) => (
            <Lines key={`${cue.start}:${cue.text}`} cue={cue} boxed={boxed} />
          ))}
        </div>
      )}
    </div>
  );
}
