import { type CSSProperties, useEffect, useState } from "react";
import type { SubtitleStyle } from "../lib/storage";
import { usePlayer } from "./context";
import type { CueTrack } from "./cues";
import { useFrame } from "./frame";
import { type Cue, cueText, type Span } from "./vtt";

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

/** Seconds of lines the native track holds ahead of the playhead, so each starts on time between `timeupdate` events. */
const AHEAD = 10;
/** The text track of each video for Safari's own full screen. A track cannot be removed, so one is kept. */
const NATIVE = new WeakMap<HTMLVideoElement, TextTrack>();

/**
 * Safari on iPhone plays full screen in its own player, which covers the page and these lines with
 * it. While it does, the same lines, shifted by `offset`, go to a text track that player draws.
 */
function useNativeLines(element: HTMLVideoElement, track: CueTrack | null, offset: number): void {
  useEffect(() => {
    let native: TextTrack | null = null;
    const shown = new Map<string, VTTCue>();
    const sync = () => {
      if (!native) return;
      const from = element.currentTime - offset;
      const wanted = new Map((track?.during(from, from + AHEAD) ?? []).map((cue) => [`${cue.start}:${cue.text}`, cue]));
      for (const [key, line] of shown) {
        if (wanted.has(key)) continue;
        native.removeCue(line);
        shown.delete(key);
      }
      for (const [key, cue] of wanted) {
        if (shown.has(key)) continue;
        const line = new VTTCue(cue.start + offset, cue.end + offset, cueText(cue));
        if (cue.place === "top") line.line = 0;
        if (cue.place === "middle") {
          line.snapToLines = false;
          line.line = 50;
        }
        native.addCue(line);
        shown.set(key, line);
      }
    };
    const begin = () => {
      native = NATIVE.get(element) ?? element.addTextTrack("subtitles", "Subtitles");
      NATIVE.set(element, native);
      native.mode = "showing";
      sync();
    };
    const end = () => {
      if (!native) return;
      for (const line of shown.values()) native.removeCue(line);
      shown.clear();
      native.mode = "disabled";
      native = null;
    };
    if ((element as HTMLVideoElement & { webkitDisplayingFullscreen?: boolean }).webkitDisplayingFullscreen) begin();
    const stop = new AbortController();
    element.addEventListener("webkitbeginfullscreen", begin, { signal: stop.signal });
    element.addEventListener("webkitendfullscreen", end, { signal: stop.signal });
    for (const name of ["timeupdate", "seeked"]) element.addEventListener(name, sync, { signal: stop.signal });
    return () => {
      stop.abort();
      end();
    };
  }, [element, track, offset]);
}

/** The subtitle lines over the picture. Follows the playhead every frame and renders when the lines change. */
export function SubtitleLayer({ track, offset, style, lifted, sample }: Props) {
  const { element } = usePlayer();
  const [cues, setCues] = useState<Cue[]>([]);
  useNativeLines(element, track, offset);
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
