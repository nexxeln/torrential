import { onlineSubtitleUrl, subtitleUrl } from "../api";
import type { SubtitleSource } from "./tracks";
import { type Cue, VttParser } from "./vtt";

/** A seek this far past the last cue read asks for the track again from there. */
const FAR = 60;

async function read(url: string, signal: AbortSignal, onCues: (cues: Cue[]) => void): Promise<void> {
  const response = await fetch(url, { signal });
  if (!response.ok || !response.body) throw new Error(`the engine answered ${response.status}`);
  const parser = new VttParser();
  const text = response.body.pipeThrough(new TextDecoderStream()).getReader();
  for (;;) {
    const { done, value } = await text.read();
    if (done) return onCues(parser.finish());
    const cues = parser.feed(value);
    if (cues.length) onCues(cues);
  }
}

/**
 * The cues of one subtitle track, loaded as the engine sends them. Files and online subtitles
 * arrive whole. An embedded track is read out of the video from the playhead on, and again from
 * the new place after a far seek. It starts where the video's own stream starts and not earlier:
 * a reader behind the video waits for pieces the video does not need, and its lines come late.
 */
export class CueTrack {
  private readonly cues = new Map<string, Cue>();
  private request = new AbortController();
  private from = 0;
  private reached = 0;
  /** Set when the track could not be loaded at all. */
  failed = false;

  constructor(
    private readonly element: HTMLVideoElement,
    private readonly id: string,
    private readonly file: number,
    private readonly source: SubtitleSource,
  ) {
    this.load(source.kind === "embedded" ? element.currentTime : 0);
    if (source.kind === "embedded") element.addEventListener("seeked", this.follow);
  }

  private url(at: number): string {
    const { source, id, file } = this;
    if (source.kind === "file") return subtitleUrl(id, source.index);
    if (source.kind === "online") return onlineSubtitleUrl(id, file, source.id);
    return subtitleUrl(id, file, { stream: source.stream, t: at });
  }

  private load(at: number): void {
    this.request.abort();
    const request = (this.request = new AbortController());
    this.from = this.reached = at;
    read(this.url(at), request.signal, (fresh) => {
      for (const cue of fresh) {
        this.cues.set(`${cue.start}:${cue.text}`, cue);
        this.reached = Math.max(this.reached, cue.start);
      }
    }).catch(() => {
      if (!request.signal.aborted) this.failed = this.cues.size === 0;
    });
  }

  private follow = (): void => {
    const time = this.element.currentTime;
    if (time < this.from - 1 || time > this.reached + FAR) this.load(time);
  };

  /** The cues to show at `time`, earliest first. */
  at(time: number): Cue[] {
    const active: Cue[] = [];
    for (const cue of this.cues.values()) if (cue.start <= time && time < cue.end) active.push(cue);
    return active.sort((a, b) => a.start - b.start);
  }

  destroy(): void {
    this.request.abort();
    this.element.removeEventListener("seeked", this.follow);
  }
}
