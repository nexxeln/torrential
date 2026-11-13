import type { AudioTrack, MediaInfo, Playback } from "../api";

/** How a file reaches the video element. */
export type Plan =
  /** The browser reads the file itself with Range requests. */
  | { kind: "direct" }
  /** The engine repackages it as fragmented MP4 for a Media Source buffer. */
  | { kind: "remux"; type: string; video: "copy" | "h264" }
  | { kind: "blocked"; reason: "ffmpeg" | "browser" };

const AAC = "mp4a.40.2";
const H264 = "avc1.640028";

type MediaSourceClass = typeof MediaSource;

/** Safari on iPhone and iPad has only the managed kind, which also evicts buffered data by itself. */
export function mediaSourceClass(): MediaSourceClass | null {
  const managed = (globalThis as { ManagedMediaSource?: MediaSourceClass }).ManagedMediaSource;
  return globalThis.MediaSource ?? managed ?? null;
}

/** The track a browser plays when it reads the file itself. */
export function defaultAudio(tracks: AudioTrack[]): AudioTrack | null {
  return tracks.find((track) => track.default) ?? tracks[0] ?? null;
}

const probe = document.createElement("video");

/**
 * `audio` is the stream the user chose, or null for the file's own choice. `direct` is false once
 * the browser has failed to read the file itself.
 */
export function plan(media: MediaInfo, audio: number | null, direct: boolean): Plan {
  const ownAudio = audio === null || audio === defaultAudio(media.audio)?.stream;
  if (direct && ownAudio && media.direct && probe.canPlayType(media.direct) !== "") return { kind: "direct" };
  if (!media.remux) return { kind: "blocked", reason: "ffmpeg" };
  const source = mediaSourceClass();
  if (!source) return { kind: "blocked", reason: "browser" };
  // A file with no audio track becomes a video-only stream, and a buffer told to expect audio would refuse it.
  const codecs = (video: string) => (media.audio.length > 0 ? `${video}, ${AAC}` : video);
  const type = (video: string) => `video/mp4; codecs="${codecs(video)}"`;
  if (media.video?.mime && source.isTypeSupported(type(media.video.mime))) {
    return { kind: "remux", type: type(media.video.mime), video: "copy" };
  }
  if (source.isTypeSupported(type(H264))) return { kind: "remux", type: type(H264), video: "h264" };
  return { kind: "blocked", reason: "browser" };
}

/** What this browser decodes, for the engine to rank a title's sources by. */
export function playback(): Playback {
  const source = mediaSourceClass();
  const decodes = (codec: string) => source?.isTypeSupported(`video/mp4; codecs="${codec}"`) ?? false;
  if (!decodes("hvc1.1.6.L120.90")) return "h264";
  return decodes("hvc1.2.4.L120.90") && matchMedia("(dynamic-range: high)").matches ? "hevc-hdr" : "hevc";
}
