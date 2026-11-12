import { type ApiError, streamUrl } from "../api";
import { mediaSourceClass } from "./plan";

export type SourceState = { kind: "streaming" } | { kind: "failed"; error: ApiError };

/** Feeds one file to a video element. The element's own events say when it plays, waits, or ends. */
export interface Source {
  /** Starts again at the current time after a failure. */
  retry(): void;
  destroy(): void;
}

export type OnState = (state: SourceState) => void;

const MEDIA_ERRORS: Record<number, string> = {
  1: "Playback was stopped.",
  2: "The connection to the engine was lost while reading the video.",
  3: "The browser could not decode this video.",
  4: "The browser cannot play this file.",
};

function mediaError(element: HTMLVideoElement): ApiError {
  const code = element.error?.code ?? 0;
  return { code: `media_error_${code}`, message: MEDIA_ERRORS[code] ?? "The video stopped with an unknown error." };
}

/**
 * Starts a freshly loaded element at `at` seconds. Setting the time before the element has
 * metadata shows the right position at once where the browser keeps it, and WebKit forgets it,
 * so it is set again when the metadata arrives.
 */
function beginAt(element: HTMLVideoElement, at: number, signal: AbortSignal, onDone?: () => void): void {
  if (at > 0) element.currentTime = at;
  element.addEventListener(
    "loadedmetadata",
    () => {
      if (at > 0 && Math.abs(element.currentTime - at) > 0.25) element.currentTime = at;
      onDone?.();
    },
    { once: true, signal },
  );
}

/** The browser reads the file itself. Seeking is a Range request, which the engine fetches first. */
export class DirectSource implements Source {
  private readonly stop = new AbortController();

  constructor(
    private readonly element: HTMLVideoElement,
    private readonly url: string,
    start: number,
    private readonly onState: OnState,
  ) {
    element.addEventListener("error", () => onState({ kind: "failed", error: mediaError(element) }), {
      signal: this.stop.signal,
    });
    this.open(start);
  }

  private open(at: number): void {
    // A file the browser reads itself can go to an AirPlay screen. A Media Source cannot.
    this.element.disableRemotePlayback = false;
    this.element.src = this.url;
    beginAt(this.element, at, this.stop.signal);
    this.onState({ kind: "streaming" });
  }

  retry(): void {
    this.open(this.element.currentTime);
  }

  destroy(): void {
    this.stop.abort();
    this.element.removeAttribute("src");
    this.element.load();
  }
}

export interface RemuxOptions {
  id: string;
  file: number;
  /** Seconds, when the file states it. */
  duration: number | null;
  /** The buffer's type, such as `video/mp4; codecs="avc1.640028, mp4a.40.2"`. */
  type: string;
  video: "copy" | "h264";
  /** The audio stream to hear. Undefined means the file's first. */
  audio: number | undefined;
  start: number;
}

/** Seconds buffered ahead of the playhead before reading the stream pauses. */
const AHEAD = 60;
const LEAST_AHEAD = 8;
/** Seconds kept behind the playhead for short seeks back. */
const BEHIND = 45;
/** A seek this close past the buffered end waits for the running stream instead of starting another. */
const NEARLY = 10;
/** Rapid seeks, as from a held arrow key, settle for this long before a stream starts. */
const SETTLE_MS = 150;
/** A gap this short before the first data is one a playing element crosses by itself. */
const TINY_GAP = 0.1;
/** A seek whose data is buffered completes well within this. */
const SEEK_STUCK_MS = 800;
const SEEK_NUDGES = 3;
/** With no stream running, a wait this long for buffered data will not end. */
const STUCK_MS = 3000;
/** A stream that ends this close to the stated duration reached the end of the file. */
const TAIL = 3;

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/**
 * Plays a file the browser cannot read itself. The engine sends it as fragmented MP4 from any
 * time, with the file's own timestamps, and this appends it to a Media Source buffer. A seek
 * outside the buffer drops what is buffered and asks for a stream from the new time.
 */
export class RemuxSource implements Source {
  private readonly media: MediaSource;
  private readonly stop = new AbortController();
  private buffer: SourceBuffer | null = null;
  /** Counts streams. Work belonging to an older stream stops when it sees a newer number. */
  private generation = 0;
  private request: AbortController | null = null;
  /** The running stream: where it began, and whether the response has ended. */
  private stream = { start: 0, ended: false };
  /** Where the last stream stopped short of the end, to tell a retry that makes no progress. */
  private shortAt: number | null = null;
  private ahead = AHEAD;
  private settle: ReturnType<typeof setTimeout> | undefined;
  private failed = false;
  /** Where playback begins, until the element has metadata and holds that time itself. */
  private beginning: number | null = null;
  /** The buffer is being emptied for a new stream. The element's own seeks in that moment are not the person's. */
  private clearing = false;
  /** A seek the person made while the buffer was being emptied, to go to once it is. */
  private redirect: number | null = null;
  private removing = false;
  private unstick: ReturnType<typeof setTimeout> | undefined;
  private nudges = 0;
  /** Why no more video will come, reported once playback has used what is buffered. */
  private shortfall: ApiError | null = null;

  constructor(
    private readonly element: HTMLVideoElement,
    private readonly options: RemuxOptions,
    private readonly onState: OnState,
  ) {
    const MediaSourceClass = mediaSourceClass()!;
    this.media = new MediaSourceClass();
    // A managed source only opens when remote playback is ruled out.
    element.disableRemotePlayback = true;
    const url = URL.createObjectURL(this.media);
    element.src = url;
    const { signal } = this.stop;
    this.beginning = options.start;
    beginAt(element, options.start, signal, () => (this.beginning = null));
    this.media.addEventListener(
      "sourceopen",
      () => {
        URL.revokeObjectURL(url);
        if (options.duration) this.media.duration = options.duration;
        this.buffer = this.media.addSourceBuffer(options.type);
        void this.load(options.start, true);
      },
      { once: true, signal },
    );
    element.addEventListener("seeking", () => this.onSeeking(), { signal });
    element.addEventListener("waiting", () => this.reportShortfall(), { signal });
    element.addEventListener("error", () => this.fail(mediaError(element)), { signal });
    onState({ kind: "streaming" });
  }

  retry(): void {
    this.restart(this.element.currentTime);
  }

  destroy(): void {
    clearTimeout(this.settle);
    clearTimeout(this.unstick);
    this.generation += 1;
    this.request?.abort();
    this.stop.abort();
    this.element.removeAttribute("src");
    this.element.load();
  }

  private fail(error: ApiError): void {
    this.failed = true;
    this.generation += 1;
    this.request?.abort();
    this.onState({ kind: "failed", error });
  }

  /**
   * What is buffered still plays. The failure shows once the playhead has nothing left ahead of
   * it, or cannot use what is there: no more data will come to complete a seek that hangs.
   */
  private reportShortfall(): void {
    if (!this.shortfall) return;
    if (this.aheadNow() < 0.5) return this.fail(this.shortfall);
    clearTimeout(this.settle);
    this.settle = setTimeout(() => {
      if (this.shortfall && this.element.readyState < HTMLMediaElement.HAVE_FUTURE_DATA) this.fail(this.shortfall);
    }, STUCK_MS);
  }

  private restart(at: number): void {
    if (!this.buffer) return;
    this.failed = false;
    this.shortAt = null;
    this.shortfall = null;
    this.onState({ kind: "streaming" });
    void this.load(at, true);
  }

  private onSeeking(): void {
    if (this.failed || !this.buffer) return;
    clearTimeout(this.settle);
    const target = this.element.currentTime;
    this.watchSeek();
    if (this.clearing) {
      // WebKit reports time zero while the buffer empties under a seek. Any other time is a new seek to honour.
      if (target !== 0) this.redirect = target;
      return;
    }
    if (this.beginning !== null) {
      // Before metadata, WebKit reports time zero for the place it was told to begin. That is not a seek.
      if (target === 0 || Math.abs(target - this.beginning) < 0.5) return;
      this.beginning = null;
    }
    if (this.covers(target)) return;
    this.settle = setTimeout(() => {
      if (!this.covers(this.element.currentTime)) this.restart(this.element.currentTime);
    }, SETTLE_MS);
  }

  /**
   * WebKit can leave a seek pending although the data for it has arrived. A seek still pending
   * after a moment, with its target buffered, is made again a hair later, which completes it.
   */
  private watchSeek(): void {
    clearTimeout(this.unstick);
    this.unstick = setTimeout(() => {
      const { element } = this;
      const { buffered, currentTime } = element;
      if (!element.seeking || this.nudges >= SEEK_NUDGES) return void (this.nudges = 0);
      for (let i = 0; i < buffered.length; i++) {
        if (buffered.start(i) <= currentTime && currentTime < buffered.end(i) - 0.2) {
          this.nudges += 1;
          element.currentTime = currentTime + 0.01;
          return;
        }
      }
    }, SEEK_STUCK_MS);
  }

  /** `time` is buffered, or the running stream is about to deliver it. */
  private covers(time: number): boolean {
    if (this.rangeAt(time)) return true;
    const { start, ended } = this.stream;
    const reached = this.rangeAt(start + 0.5)?.end ?? start;
    return !ended && time >= start - 0.5 && time <= reached + NEARLY;
  }

  private rangeAt(time: number): { start: number; end: number } | null {
    const { buffered } = this.element;
    for (let i = 0; i < buffered.length; i++) {
      if (time >= buffered.start(i) - 0.1 && time < buffered.end(i)) return { start: buffered.start(i), end: buffered.end(i) };
    }
    return null;
  }

  private bufferedEnd(): number {
    const { buffered } = this.element;
    return buffered.length ? buffered.end(buffered.length - 1) : 0;
  }

  private async idle(buffer: SourceBuffer): Promise<void> {
    while (buffer.updating) await new Promise((resolve) => buffer.addEventListener("updateend", resolve, { once: true }));
  }

  private async remove(buffer: SourceBuffer, from: number, to: number): Promise<void> {
    await this.idle(buffer);
    if (to <= from || this.media.readyState === "closed") return;
    buffer.remove(from, to);
    this.removing = true;
    try {
      await this.idle(buffer);
    } finally {
      this.removing = false;
    }
  }

  /** Requests the file from `at` and appends it until the response ends or a newer stream replaces this one. */
  private async load(at: number, clear: boolean): Promise<void> {
    const buffer = this.buffer!;
    const generation = ++this.generation;
    const current = () => generation === this.generation;
    this.request?.abort();
    const request = (this.request = new AbortController());
    this.stream = { start: at, ended: false };
    try {
      // A removal cannot be aborted, so it is waited out.
      if (this.removing) await this.idle(buffer);
      if (!current()) return;
      // Dropping a stream mid-fragment leaves the parser inside it; abort() returns it to the start.
      if (this.media.readyState === "open") buffer.abort();
      if (clear) {
        this.clearing = true;
        this.redirect = null;
        try {
          await this.remove(buffer, 0, Infinity);
        } finally {
          this.clearing = false;
        }
        if (!current()) return;
        if (this.redirect !== null) return void this.load(this.redirect, true);
        // The element can lose its place while the buffer empties. This stream is for `at`.
        if (this.beginning === null && Math.abs(this.element.currentTime - at) > 0.5) this.element.currentTime = at;
      }
      const { id, file, video, audio } = this.options;
      const response = await fetch(streamUrl(id, file, { t: at, video, audio }), { signal: request.signal });
      if (!response.ok || !response.body) {
        const body = (await response.json().catch(() => null)) as { error?: ApiError } | null;
        throw body?.error ?? { code: "internal", message: `The engine answered ${response.status} for the video stream.` };
      }
      const reader = response.body.getReader();
      for (;;) {
        while (current() && this.aheadNow() > this.ahead) await sleep(250);
        if (!current()) return void reader.cancel().catch(() => {});
        const { done, value } = await reader.read();
        if (!current()) return void reader.cancel().catch(() => {});
        if (done) break;
        if (!(await this.append(buffer, value, current))) return;
        this.skipGap();
        await this.trim(buffer);
        if (!current()) return;
      }
      await this.finish(buffer, at, current);
    } catch (error) {
      if (!current() || request.signal.aborted) return;
      const known = typeof error === "object" && error !== null && "code" in error && "message" in error;
      this.fail(
        known
          ? (error as ApiError)
          : { code: "stream_failed", message: "The video stream from the engine stopped.", hint: String(error) },
      );
    }
  }

  private aheadNow(): number {
    const range = this.rangeAt(this.element.currentTime);
    return range ? range.end - this.element.currentTime : 0;
  }

  /** False when a newer stream took over while this chunk waited. */
  private async append(buffer: SourceBuffer, chunk: Uint8Array<ArrayBuffer>, current: () => boolean): Promise<boolean> {
    for (let attempt = 0; ; attempt++) {
      await this.idle(buffer);
      if (!current()) return false;
      try {
        buffer.appendBuffer(chunk);
        await this.idle(buffer);
        return current();
      } catch (error) {
        const full = error instanceof DOMException && error.name === "QuotaExceededError";
        if (!full || attempt >= 20) throw error;
        // The buffer holds less of a high-bitrate video than AHEAD seconds. Keep less, and wait for playback to use some.
        this.ahead = Math.max(LEAST_AHEAD, Math.min(this.ahead, this.aheadNow()) * 0.7);
        await this.remove(buffer, 0, Math.max(0, this.element.currentTime - 5));
        while (current() && this.aheadNow() > this.ahead) await sleep(250);
        if (attempt > 2) await sleep(1000);
      }
    }
  }

  /**
   * Data starts a moment after the time asked for when no frame sits exactly there, and the
   * playhead has to step onto it. A gap of a few hundredths is different: a playing element
   * crosses it by itself, and seeking then can stall WebKit, while a paused one waits at it forever.
   */
  private skipGap(): void {
    const { buffered, currentTime, paused } = this.element;
    if (this.beginning !== null) return;
    for (let i = 0; i < buffered.length; i++) {
      if (buffered.start(i) <= currentTime && currentTime < buffered.end(i)) return;
      const gap = buffered.start(i) - currentTime;
      if (gap > (paused ? 0 : TINY_GAP) && gap < 1.5 && buffered.end(i) - buffered.start(i) > 0.2) {
        this.element.currentTime = buffered.start(i) + 0.01;
        return;
      }
    }
  }

  private async trim(buffer: SourceBuffer): Promise<void> {
    const { buffered, currentTime } = this.element;
    if (buffered.length && buffered.start(0) < currentTime - BEHIND * 2) {
      await this.remove(buffer, 0, currentTime - BEHIND);
    }
  }

  /** The response ended: at the end of the file, or short of it when ffmpeg stopped early. */
  private async finish(buffer: SourceBuffer, at: number, current: () => boolean): Promise<void> {
    // A stream with no data, as from a seek to the last frames, still reached the time it began at.
    const end = Math.max(at, this.bufferedEnd());
    const { duration } = this.options;
    const complete = duration === null || end >= duration - TAIL;
    if (!complete && (this.shortAt === null || end > this.shortAt + 1)) {
      // Ask for the rest once. Fragments carry real timestamps, so they join what is buffered.
      this.shortAt = end;
      return this.load(Math.max(at, end - 0.5), false);
    }
    const nearEnd = duration !== null && end >= duration - Math.max(60, duration * 0.02);
    if (!complete && !nearEnd) {
      this.shortfall = {
        code: "stream_ended_early",
        message: "The video stream stopped before the end of the file.",
        hint: "The file may be damaged, or ffmpeg cannot read past this point.",
      };
      return this.reportShortfall();
    }
    this.stream.ended = true;
    await this.idle(buffer);
    if (current() && this.media.readyState === "open") this.media.endOfStream();
  }
}
