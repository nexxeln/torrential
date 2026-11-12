// The engine's HTTP API, served on the same origin as this app (http://127.0.0.1:47919 by default).
//
// Every JSON type here mirrors a Rust type in crates/core or crates/torrential/src/daemon/web.
// A failed request answers with a 4xx or 5xx status and `{ "error": ApiError }`.
//
//   GET  /api/search?q=&category=&limit=&fresh=1        NDJSON stream of SearchMessage
//   POST /api/play        { ref, file? }                PlayInfo
//   GET  /api/media?id=&file=                           MediaInfo
//   GET  /api/stream?id=&file=&t=&audio=&video=         fragmented MP4, see streamUrl
//   GET  /api/subtitles?id=&file=[&stream=&t=]          WebVTT, see subtitleUrl
//   GET  /api/subtitles/online?id=&file=                 OnlineSubtitles
//   GET  /api/subtitles/online/fetch?id=&file=&subtitle= WebVTT, see onlineSubtitleUrl
//   GET  /api/thumbnail?id=&file=&t=                    JPEG, see thumbnailUrl
//   GET  /api/pieces?id=&file=                          FilePieces
//   GET  /api/events                                    server-sent events, each `data:` a JobEvent
//   GET  /api/jobs                                      { jobs: Job[] }
//   GET  /api/files?ref=                                FileListing
//   POST /api/pause | /api/resume | /api/remove  { refs }   { changes: Change[] }
//   GET  /{id}/{file index}/{name}                      the file's bytes, with Range support

export type Category = "video" | "audio" | "software" | "book" | "game" | "dataset" | "other";
export type Flag = "official" | "trusted" | "public_domain" | "creative_commons" | "cam" | "adult";
export type Resolution = "sd" | "720p" | "1080p" | "2160p";
export type NameCodec = "h264" | "hevc" | "av1";

/** What a release's name says about its video. */
export interface Quality {
  resolution: Resolution | null;
  codec: NameCodec | null;
  hdr: boolean;
}

export interface Release {
  /** A 40-hex infohash, or `u` and a digest for a result that only links a .torrent file. Pass it as `ref`. */
  id: string;
  name: string;
  size: number | null;
  seeders: number | null;
  leechers: number | null;
  /** Unix seconds. */
  published: number | null;
  category: Category | null;
  quality: Quality;
  flags: Flag[];
  /** Sources that listed it, most authoritative first. */
  sources: string[];
  magnet: string | null;
  torrent_url: string | null;
  info_url: string | null;
}

export type SourceReport = { source: string; elapsed_ms: number } & (
  | { outcome: "ok" | "cached"; results: number }
  | { outcome: "timeout" | "irrelevant" }
  | { outcome: "http"; status: number }
  | { outcome: "connect" | "bad_response"; reason: string }
);

export interface SearchResult {
  /** Ranked by the engine for this machine. Show them in this order. */
  releases: Release[];
  /** Matching releases before the limit. */
  total: number;
  hidden_adult: number;
  sources: SourceReport[];
  elapsed_ms: number;
}

/** One line of the /api/search stream. `update` repeats with the results so far as each source answers. */
export type SearchMessage =
  | { type: "update"; result: SearchResult; pending: string[] }
  | { type: "done"; result: SearchResult }
  | { type: "error"; error: ApiError };

export interface ApiError {
  /** A stable snake_case code such as `not_found`, `metadata_timeout`, or `no_source`. */
  code: string;
  message: string;
  hint?: string;
}

export type JobState =
  | "queued"
  | "metadata"
  | "checking"
  | "downloading"
  | "paused"
  | "done"
  | "interrupted"
  | "error";

export interface Progress {
  /** Bytes selected for download. Null until the file list arrives. */
  size: number | null;
  verified: number;
  /**
   * 64 characters, one per equal run of the torrent's pieces in order: `0` to `8` for the eighths
   * of the run that arrived, `-` for a run with nothing selected. Absent before the first sample
   * and once the download is done.
   */
  piece_map?: string;
  /** Bytes per second. */
  down_rate: number;
  up_rate: number;
  peers: number;
  seeds: number;
  /** Seconds, or null while the rate is unknown. */
  eta: number | null;
}

export interface Job {
  id: string;
  name: string | null;
  dir: string;
  path: string | null;
  state: JobState;
  /** With `state: "done"`. Unix seconds. */
  finished_at?: number;
  /** With `state: "error"`. */
  error?: ApiError;
  progress: Progress;
  /** Unix seconds. */
  added_at: number;
}

export type JobEvent =
  | { event: "snapshot"; jobs: Job[] }
  | { event: "job"; job: Job }
  | { event: "removed"; id: string }
  | { event: "heartbeat"; at: number };

export interface TorrentFile {
  index: number;
  /** Relative to the torrent's folder. */
  path: string;
  size: number;
  selected: boolean;
  verified: number;
  risk?: "executable";
}

export interface FileListing {
  id: string;
  name: string | null;
  total_size: number;
  files: TorrentFile[];
}

export interface Change {
  id: string;
  name: string | null;
  result: string;
}

/** A file the engine serves while it downloads. */
export interface StreamedFile {
  index: number;
  path: string;
  size: number;
  /** Same-origin path to the file's bytes. */
  url: string;
  /** Where the file sits in the torrent, to read its part of `Progress.piece_map`. */
  offset: number;
}

/** The answer to POST /api/play: the download has started and `video` is being fetched in playing order. */
export interface PlayInfo {
  id: string;
  name: string;
  job: Job;
  /** Every file in the torrent, so a season can list its other episodes. */
  files: TorrentFile[];
  /** The torrent's total bytes, the length `Progress.piece_map` spans. */
  total_size: number;
  video: StreamedFile;
  /** Subtitle files in the torrent that belong with `video`. */
  subtitles: StreamedFile[];
}

export interface VideoTrack {
  stream: number;
  /** ffmpeg's name: `h264`, `hevc`, `av1`, `vp9`, `mpeg4`, ... */
  codec: string;
  /** The RFC 6381 codec string, such as `avc1.640028` or `hvc1.2.4.L150.B0`. Null when MP4 cannot carry the codec. */
  mime: string | null;
  width: number;
  height: number;
  bit_depth: number;
  hdr: boolean;
  fps: number | null;
}

export interface AudioTrack {
  stream: number;
  /** ffmpeg's name: `aac`, `ac3`, `eac3`, `dts`, `truehd`, `opus`, `flac`, ... */
  codec: string;
  channels: number;
  /** ISO 639-2, such as `eng`. */
  language: string | null;
  title: string | null;
  default: boolean;
}

export interface SubtitleTrack {
  stream: number;
  /** ffmpeg's name: `subrip`, `ass`, `webvtt`, `hdmv_pgs_subtitle`, ... */
  codec: string;
  language: string | null;
  title: string | null;
  default: boolean;
  forced: boolean;
  /** False for picture subtitles (PGS, VobSub), which cannot become WebVTT. */
  text: boolean;
}

/**
 * What is inside a video file. The request waits for the first and last pieces of the file to
 * arrive, so it can take a while on a slow swarm.
 */
export interface MediaInfo {
  /** Seconds. */
  duration: number | null;
  /** `mp4`, `matroska`, `webm`, `avi`, `mpegts`, ... */
  container: string;
  video: VideoTrack | null;
  audio: AudioTrack[];
  subtitles: SubtitleTrack[];
  /**
   * A full type such as `video/mp4; codecs="avc1.640028, mp4a.40.2"` when the file's container is
   * one browsers read. If `canPlayType` accepts it, play the file's own URL.
   */
  direct: string | null;
  /** ffmpeg is installed, so /api/stream, embedded subtitles, and thumbnails work. */
  remux: boolean;
  /** The file's own chapters, in order. Empty for most files. */
  chapters: Chapter[];
}

export interface Chapter {
  /** Seconds. */
  start: number;
  end: number;
  title: string | null;
}

/** A subtitle found online for this video. It is not part of the torrent. */
export interface OnlineSubtitle {
  /** Pass to onlineSubtitleUrl. */
  id: string;
  /** ISO 639-2, such as `eng`, `fre`, or `pob`. */
  language: string;
  /** The subtitle's file or release name, to tell versions apart. */
  name: string | null;
  /** Made for this exact file, so its timing fits. Others can be early or late. */
  exact: boolean;
  /** Where it comes from, such as `opensubtitles`. */
  source: string;
}

/** The film or episode the engine read from the torrent's names, and found online. */
export interface OnlineTitle {
  name: string;
  year: number | null;
  season: number | null;
  episode: number | null;
}

/**
 * Subtitles for the video from online sources, best first within each language. `title` is null
 * and the list empty when the engine cannot tell what the video is, when no source answers, or
 * when config.yaml turns online subtitles off.
 */
export interface OnlineSubtitles {
  title: OnlineTitle | null;
  subtitles: OnlineSubtitle[];
}

/**
 * Which parts of one file have downloaded: 200 characters, one per equal run of the file's bytes
 * in order, `0` to `8` for the eighths of the run that arrived.
 */
export interface FilePieces {
  pieces: string;
}

export class EngineError extends Error {
  constructor(
    readonly error: ApiError,
    readonly status: number,
  ) {
    super(error.message);
  }
}

/** The engine's error for anything a request threw, including a network failure when the engine is not running. */
export function asApiError(thrown: unknown): ApiError {
  if (thrown instanceof EngineError) return thrown.error;
  return {
    code: "engine_unreachable",
    message: "Cannot reach the torrential engine.",
    hint: "Run torrential web in a terminal to start it, then try again.",
  };
}

async function json<T>(response: Response): Promise<T> {
  const body = await response.json().catch(() => null);
  if (!response.ok) {
    const error = body?.error ?? { code: "internal", message: `the engine answered ${response.status}` };
    throw new EngineError(error, response.status);
  }
  return body as T;
}

function post<T>(path: string, body: unknown, signal?: AbortSignal): Promise<T> {
  const init = { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), signal };
  return fetch(path, init).then(json<T>);
}

/** Yields the results so far each time a source answers. The last message is `done` or `error`. */
export async function* search(
  query: string,
  options: { category?: Category; limit?: number; fresh?: boolean; signal?: AbortSignal } = {},
): AsyncGenerator<SearchMessage> {
  const params = new URLSearchParams({ q: query });
  if (options.category) params.set("category", options.category);
  if (options.limit) params.set("limit", String(options.limit));
  if (options.fresh) params.set("fresh", "1");
  const response = await fetch(`/api/search?${params}`, { signal: options.signal });
  if (!response.ok || !response.body) {
    yield { type: "error", error: await json<never>(response).catch((e: EngineError) => e.error) };
    return;
  }
  const lines = response.body.pipeThrough(new TextDecoderStream()).getReader();
  let rest = "";
  for (;;) {
    const { done, value } = await lines.read();
    if (done) return;
    rest += value;
    for (let end = rest.indexOf("\n"); end !== -1; end = rest.indexOf("\n")) {
      const line = rest.slice(0, end).trim();
      rest = rest.slice(end + 1);
      if (line) yield JSON.parse(line) as SearchMessage;
    }
  }
}

/**
 * Starts or joins the download of `ref` (a Release or Job ID, magnet link, infohash, or .torrent URL)
 * and fetches one video first. Without `file`, the engine picks the main video. Waits for the file
 * list, up to a minute for a magnet.
 */
export function play(ref: string, file?: number, signal?: AbortSignal): Promise<PlayInfo> {
  return post("/api/play", { ref, file }, signal);
}

export function media(id: string, file: number, signal?: AbortSignal): Promise<MediaInfo> {
  return fetch(`/api/media?id=${id}&file=${file}`, { signal }).then(json<MediaInfo>);
}

/**
 * A fragmented MP4 of the file from `t` seconds on, for a Media Source buffer. Its timestamps are
 * the file's own: video starts at the keyframe at or before `t`, audio at `t`. The audio is AAC
 * (`mp4a.40.2`). The video is the file's own with `video: "copy"`, or H.264 High (`avc1.640028`)
 * with `video: "h264"` for a codec the browser cannot decode.
 */
export function streamUrl(
  id: string,
  file: number,
  options: { t: number; audio?: number; video: "copy" | "h264" },
): string {
  const params = new URLSearchParams({ id, file: String(file), t: options.t.toFixed(3), video: options.video });
  if (options.audio !== undefined) params.set("audio", String(options.audio));
  return `/api/stream?${params}`;
}

/**
 * WebVTT, sent as cues are read. With `stream`, the subtitle track with that stream index inside video
 * `file`, from `t` seconds on. Without it, `file` is a subtitle file of the torrent, converted whole.
 */
export function subtitleUrl(id: string, file: number, embedded?: { stream: number; t: number }): string {
  const params = new URLSearchParams({ id, file: String(file) });
  if (embedded) {
    params.set("stream", String(embedded.stream));
    params.set("t", embedded.t.toFixed(3));
  }
  return `/api/subtitles?${params}`;
}

export function onlineSubtitles(id: string, file: number, signal?: AbortSignal): Promise<OnlineSubtitles> {
  return fetch(`/api/subtitles/online?id=${id}&file=${file}`, { signal }).then(json<OnlineSubtitles>);
}

/** WebVTT of one OnlineSubtitle of video `file`, whole. */
export function onlineSubtitleUrl(id: string, file: number, subtitle: string): string {
  return `/api/subtitles/online/fetch?${new URLSearchParams({ id, file: String(file), subtitle })}`;
}

/**
 * A JPEG of the frame at `t` seconds, 240 pixels wide. Ask only for whole multiples of 10 seconds,
 * so answers are shared, and only where the file has downloaded: the engine answers 404 rather
 * than fetch pieces for a picture.
 */
export function thumbnailUrl(id: string, file: number, t: number): string {
  return `/api/thumbnail?${new URLSearchParams({ id, file: String(file), t: String(Math.floor(t / 10) * 10) })}`;
}

export function pieces(id: string, file: number, signal?: AbortSignal): Promise<FilePieces> {
  return fetch(`/api/pieces?id=${id}&file=${file}`, { signal }).then(json<FilePieces>);
}

export function jobs(signal?: AbortSignal): Promise<Job[]> {
  return fetch("/api/jobs", { signal })
    .then(json<{ jobs: Job[] }>)
    .then((body) => body.jobs);
}

export function files(ref: string, signal?: AbortSignal): Promise<FileListing> {
  return fetch(`/api/files?ref=${encodeURIComponent(ref)}`, { signal }).then(json<FileListing>);
}

export function change(action: "pause" | "resume" | "remove", refs: string[]): Promise<Change[]> {
  return post<{ changes: Change[] }>(`/api/${action}`, { refs }).then((body) => body.changes);
}

/**
 * Follows every download. The first event is a snapshot. `onConnection` hears when the stream to the
 * engine opens and when it is lost; the browser reconnects by itself. Returns a function that stops following.
 */
export function watchJobs(onEvent: (event: JobEvent) => void, onConnection?: (connected: boolean) => void): () => void {
  const source = new EventSource("/api/events");
  source.onmessage = (message) => onEvent(JSON.parse(message.data) as JobEvent);
  source.onopen = () => onConnection?.(true);
  source.onerror = () => onConnection?.(false);
  return () => source.close();
}
