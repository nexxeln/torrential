import type { Chapter } from "../api";
import { clock } from "../lib/format";
import { savePreferences } from "../lib/storage";
import type { FeedbackStore } from "./feedback";

export const SPEEDS = [0.25, 0.5, 0.75, 1, 1.25, 1.5, 1.75, 2, 2.5, 3];

/** Going back from this early in a chapter goes to the one before it. */
const CHAPTER_GRACE = 3;

/** What the video is, which the element does not know by itself. */
export interface Timeline {
  /** Seconds. */
  duration: number | null;
  /** Frames per second, for stepping. */
  fps: number | null;
  chapters: Chapter[];
}

/** Everything a key, a button, or a media key can ask of the video, each with its on-screen answer. */
export class PlayerActions {
  constructor(
    private readonly element: HTMLVideoElement,
    private readonly feedback: FeedbackStore,
    private readonly timeline: () => Timeline,
  ) {}

  play = (): void => void this.element.play().catch(() => {});

  /** `shown` draws the play or pause symbol over the picture, for actions with no button under the pointer. */
  toggle = (shown = false): void => {
    const start = this.element.paused || this.element.ended;
    if (start) this.play();
    else this.element.pause();
    if (shown) this.feedback.bezel(start ? "play" : "pause");
  };

  private land(time: number): number {
    const { duration } = this.timeline();
    return Math.min(Math.max(0, time), Math.max(0, (duration ?? Infinity) - 0.25));
  }

  seekTo = (time: number): void => {
    this.element.currentTime = this.land(time);
  };

  seekBy = (seconds: number): void => {
    this.seekTo(this.element.currentTime + seconds);
    this.feedback.seek(seconds);
  };

  seekToFraction = (fraction: number): void => {
    const { duration } = this.timeline();
    if (!duration) return;
    this.seekTo(duration * fraction);
    this.feedback.note(clock(this.land(duration * fraction)));
  };

  /** One frame forward or back, while paused. */
  step = (direction: 1 | -1): void => {
    if (!this.element.paused) return;
    const frame = 1 / (this.timeline().fps ?? 24);
    this.seekTo(this.element.currentTime + direction * frame);
    const time = this.element.currentTime;
    this.feedback.note(`${clock(time)}.${String(Math.floor((time % 1) * 1000)).padStart(3, "0")}`);
  };

  chapter = (direction: 1 | -1): void => {
    const { chapters } = this.timeline();
    const time = this.element.currentTime;
    const now = chapters.findIndex((chapter) => chapter.start <= time && time < chapter.end);
    if (chapters.length === 0 || now === -1) return this.feedback.note("This video has no chapters");
    const restart = direction === -1 && time - chapters[now]!.start > CHAPTER_GRACE;
    const index = restart ? now : now + direction;
    const target = chapters[index];
    if (!target) return this.feedback.note(direction === 1 ? "This is the last chapter" : "This is the first chapter");
    this.seekTo(target.start);
    this.feedback.note(target.title ?? `Chapter ${index + 1}`);
  };

  private saveVolume(): void {
    const { volume, muted } = this.element;
    savePreferences({ volume, muted });
    this.feedback.note(muted || volume === 0 ? "Muted" : `Volume ${Math.round(volume * 100)}%`, muted ? 0 : volume);
  }

  setVolume = (volume: number): void => {
    this.element.volume = Math.min(1, Math.max(0, Math.round(volume * 100) / 100));
    this.element.muted = this.element.volume === 0;
    this.saveVolume();
  };

  volumeBy = (amount: number): void => this.setVolume((this.element.muted ? 0 : this.element.volume) + amount);

  toggleMute = (): void => {
    this.element.muted = !this.element.muted;
    if (!this.element.muted && this.element.volume === 0) this.element.volume = 0.5;
    this.saveVolume();
  };

  setSpeed = (speed: number): void => {
    this.element.defaultPlaybackRate = this.element.playbackRate = speed;
    savePreferences({ speed });
    this.feedback.note(speed === 1 ? "Normal speed" : `${speed}× speed`);
  };

  speedBy = (steps: number): void => {
    const at = SPEEDS.indexOf(this.element.playbackRate);
    const from = at === -1 ? SPEEDS.indexOf(1) : at;
    this.setSpeed(SPEEDS[Math.min(SPEEDS.length - 1, Math.max(0, from + steps))]!);
  };

  togglePictureInPicture = (): void => {
    if (document.pictureInPictureElement) void document.exitPictureInPicture();
    else void this.element.requestPictureInPicture().catch(() => {});
  };
}
