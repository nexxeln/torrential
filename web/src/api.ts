// The engine's HTTP API, served on the same origin as this app (http://127.0.0.1:47919 by default,
// or the Mac's address on the home network for a paired phone or TV).
//
// Every JSON type comes from wire.ts, which `cargo test` writes from the Rust types that make the JSON.
// A failed request answers with a 4xx or 5xx status and `{ "error": ApiError }`.
//
//   POST /api/search      { q, category?, limit?, fresh? }   NDJSON stream of SearchMessage
//   POST /api/play        { ref, file? }                PlayInfo
//   GET  /api/media?id=&file=                           MediaInfo
//   GET  /api/stream?id=&file=&t=&audio=&video=         fragmented MP4, see streamUrl
//   GET  /api/subtitles?id=&file=[&stream=&t=]          WebVTT, see subtitleUrl
//   GET  /api/subtitles/online?id=&file=                 OnlineSubtitles
//   GET  /api/subtitles/online/fetch?id=&file=&subtitle= WebVTT, see onlineSubtitleUrl
//   GET  /api/thumbnail?id=&file=&t=                    JPEG, see thumbnailUrl
//   POST /api/ffmpeg      {}                            NDJSON stream of FfmpegMessage
//   GET  /api/pieces?id=&file=                          FilePieces
//   GET  /api/events                                    server-sent events, each `data:` a JobEvent
//   GET  /api/jobs                                      { jobs: Job[] }
//   POST /api/files       { ref }                       FilesReply
//   POST /api/pause | /api/resume  { refs }               { changes: Change[] }
//   POST /api/remove      { refs, delete? }              { changes: Change[] }
//   GET  /{id}/{file index}/{name}                      the file's bytes, with Range support
//
// Films and series, by IMDb ID:
//
//   GET  /api/titles?q=                                    TitleSearch
//   GET  /api/title?id=                                    TitlePage
//   GET  /api/title/art?id=&kind=[&season=&episode=]       an image, see artUrl
//   GET  /api/title/sources?video=[&width=&height=&scale=&playback=]   NDJSON stream of SearchMessage
//   POST /api/title/play     PlayTitle                     NDJSON stream of PlayMessage
//   POST /api/watched        { video, position, duration, release? }   { watched: Watched, kept: boolean }
//   POST /api/library        { title, saved }              { saved: boolean }
//   GET  /api/home                                         Home
//
// Phones and TVs. A browser on the home network that is not paired gets 401 from every route, and
// /pair for every page. Only the Mac itself may add and remove devices:
//
//   GET  /api/me                                           Me
//   GET  /api/devices                                      DeviceList
//   POST /api/devices/add                                  DeviceList, with a new offer
//   POST /api/devices/remove   { id }                      DeviceList
//
// The engine:
//
//   GET  /api/version                                      Version
//
// Play on TV. A paired phone may cast and work the remote too:
//
//   GET  /api/tvs                                          TvList, as known now
//   POST /api/tvs              { fresh? }                  TvList, after a search of the home network
//   POST /api/cast             CastStart                   { cast: Cast }, at once
//   POST /api/cast/control     { tv?, ...CastAction }       { cast: Cast }

import type {
  AttemptEvent,
  AudioTrack,
  Cast,
  CastAction,
  CastStart,
  CastState,
  Category,
  Change,
  Chapter,
  Check,
  Codec,
  Container,
  Continue,
  Country,
  Device,
  DeviceList,
  EpisodeInfo,
  Error,
  ErrorCode,
  Event,
  FfmpegMessage,
  FilePieces,
  FilesReply,
  Flag,
  Home,
  Job,
  Kind,
  Me,
  MediaInfo,
  Next,
  OnlineSubtitle,
  OnlineSubtitles,
  OnlineTitle,
  PairingOffer,
  PlayInfo,
  Playback,
  PlayingLine,
  PlayTitle,
  Progress,
  Quality,
  Release,
  Resolution,
  SearchMessage as EngineSearchMessage,
  SearchResult,
  Season,
  Skip,
  SourceReport,
  StreamedFile,
  SubtitleTrack,
  Swarm,
  TitleCard,
  TitlePage,
  TitleSearch,
  TorrentFile,
  Tv,
  TvList,
  Version,
  VideoTrack,
  Viewer,
  Watched,
} from "./wire";

export type {
  AudioTrack,
  Cast,
  CastAction,
  CastStart,
  CastState,
  Category,
  Change,
  Chapter,
  Check,
  Codec,
  Container,
  Continue,
  Country,
  Device,
  DeviceList,
  EpisodeInfo,
  ErrorCode,
  FfmpegMessage,
  FilePieces,
  FilesReply,
  Flag,
  Home,
  Job,
  Kind,
  Me,
  MediaInfo,
  Next,
  OnlineSubtitle,
  OnlineSubtitles,
  OnlineTitle,
  PairingOffer,
  PlayInfo,
  Playback,
  PlayTitle,
  Progress,
  Quality,
  Release,
  Resolution,
  SearchResult,
  Season,
  Skip,
  SourceReport,
  StreamedFile,
  SubtitleTrack,
  Swarm,
  TitleCard,
  TitlePage,
  TitleSearch,
  TorrentFile,
  Tv,
  TvList,
  Version,
  VideoTrack,
  Viewer,
  Watched,
};

/** A failed request's error: the engine's, or one this app names when the engine cannot answer. */
export type ApiError = Omit<Error, "code"> & { code: ErrorCode | AppErrorCode };

/** The codes this app gives what fails in the browser, beside the engine's. */
type AppErrorCode = "engine_unreachable" | "stream_failed" | "stream_ended_early" | `media_error_${number}`;

/** One line of the /api/search stream. `update` repeats with the results so far as each source answers. */
export type SearchMessage = Exclude<EngineSearchMessage, { type: "error" }> | { type: "error"; error: ApiError };

export type JobState = Job["state"];

/** The events of GET /api/events. */
export type JobEvent = Exclude<Event, { event: "search_update" }>;

/** One line of the /api/title/play stream. The last is `playing` or `failed`. */
export type PlayMessage = Exclude<AttemptEvent, { type: "playing" }> | PlayingLine;

/** What of a title a release holds: `film`, `s3e5`, `s3`, `s1-5`, or `complete`. */
export type Span = string;

export class EngineError extends Error {
  constructor(
    readonly error: ApiError,
    readonly status: number,
  ) {
    super(error.message);
  }
}

const UNREACHABLE: ApiError = {
  code: "engine_unreachable",
  message: "Cannot reach the torrential engine.",
  hint: "Run torrential web in a terminal to start it, then try again.",
};

/** The engine's error for anything a request threw, including a network failure when the engine is not running. */
export function asApiError(thrown: unknown): ApiError {
  return thrown instanceof EngineError ? thrown.error : UNREACHABLE;
}

/** A body that stops arriving, as when its request is aborted, rejects instead of answering null. */
async function json<T>(response: Response): Promise<T> {
  if (response.ok) return response.json() as Promise<T>;
  // A phone or TV that was removed on the Mac goes to the page that pairs it again.
  if (response.status === 401) location.assign("/pair");
  const body = await response.json().catch(() => null);
  // The engine names every failure. A server error without a name is a proxy in front of an engine that is not running.
  const unnamed = response.status >= 500 ? UNREACHABLE : { code: "internal", message: `the engine answered ${response.status}` };
  throw new EngineError(body?.error ?? unnamed, response.status);
}

/** The JSON values of an NDJSON body, one per line, as they arrive. */
async function* lines<T>(response: Response): AsyncGenerator<T> {
  const reader = response.body!.pipeThrough(new TextDecoderStream()).getReader();
  let rest = "";
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      rest += value;
      for (let end = rest.indexOf("\n"); end !== -1; end = rest.indexOf("\n")) {
        const line = rest.slice(0, end).trim();
        rest = rest.slice(end + 1);
        if (line) yield JSON.parse(line) as T;
      }
    }
  } finally {
    // A reader that stops early hangs up, so the engine stops what the stream was for.
    void reader.cancel().catch(() => {});
  }
}

/** A search stream. A failed request becomes its `error` message. */
async function* searchMessages(path: string, init: RequestInit): AsyncGenerator<SearchMessage> {
  const response = await fetch(path, init);
  if (!response.ok || !response.body) {
    yield { type: "error", error: await json<never>(response).catch((e: EngineError) => e.error) };
    return;
  }
  yield* lines<SearchMessage>(response);
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
  const { signal, ...rest } = options;
  const body = JSON.stringify({ q: query, ...rest });
  yield* searchMessages("/api/search", { method: "POST", headers: { "content-type": "application/json" }, body, signal });
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

/** Has the engine fetch ffmpeg, saying how far the download is. The fetch carries on when this stops. */
export async function* fetchFfmpeg(signal?: AbortSignal): AsyncGenerator<FfmpegMessage> {
  const init = { method: "POST", headers: { "content-type": "application/json" }, body: "{}", signal };
  const response = await fetch("/api/ffmpeg", init);
  if (!response.ok || !response.body) await json<never>(response);
  yield* lines<FfmpegMessage>(response);
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

export function files(ref: string, signal?: AbortSignal): Promise<FilesReply> {
  return post("/api/files", { ref }, signal);
}

export function me(): Promise<Me> {
  return fetch("/api/me").then(json<Me>);
}

export function devices(): Promise<DeviceList> {
  return fetch("/api/devices").then(json<DeviceList>);
}

/** Opens a new code in place of any open one. */
export function addDevice(): Promise<DeviceList> {
  return post("/api/devices/add", {});
}

/** Locks a device out at once. */
export function removeDevice(id: string): Promise<DeviceList> {
  return post("/api/devices/remove", { id });
}

/** With `remove`, `deleteFiles` moves the files of a download in a folder the person chose to the Trash too. */
export function change(action: "pause" | "resume" | "remove", refs: string[], deleteFiles = false): Promise<Change[]> {
  return post<{ changes: Change[] }>(`/api/${action}`, { refs, delete: deleteFiles }).then((body) => body.changes);
}

/**
 * Follows every download. The first event is a snapshot. `onConnection` hears when the stream to the
 * engine opens and when it is lost; the browser reconnects by itself. Returns a function that stops following.
 */
export function watchJobs(onEvent: (event: JobEvent) => void, onConnection?: (connected: boolean) => void): () => void {
  const source = new EventSource("/api/events");
  source.onmessage = (message) => onEvent(JSON.parse(message.data) as JobEvent);
  source.onopen = () => onConnection?.(true);
  source.onerror = () => {
    onConnection?.(false);
    // The browser gives up on a stream the engine refused, which it does once a device is removed.
    if (source.readyState === EventSource.CLOSED) void me().catch(() => {});
  };
  return () => source.close();
}

export function titles(query: string, signal?: AbortSignal): Promise<TitleSearch> {
  return fetch(`/api/titles?${new URLSearchParams({ q: query })}`, { signal }).then(json<TitleSearch>);
}

export function title(id: string, signal?: AbortSignal): Promise<TitlePage> {
  return fetch(`/api/title?${new URLSearchParams({ id })}`, { signal }).then(json<TitlePage>);
}

/** A same-origin image URL. Answers 404 when the catalog has no such picture. */
export function artUrl(id: string, kind: "poster" | "background" | "logo" | { season: number; episode: number }): string {
  const params =
    typeof kind === "string"
      ? new URLSearchParams({ id, kind })
      : new URLSearchParams({ id, kind: "thumbnail", season: String(kind.season), episode: String(kind.episode) });
  return `/api/title/art?${params}`;
}

/** The releases of one film or episode, ranked for playing in this browser, as each source answers. The last message is `done` or `error`. */
export async function* sources(video: string, viewer?: Viewer, signal?: AbortSignal): AsyncGenerator<SearchMessage> {
  const params = new URLSearchParams({ video });
  if (viewer) for (const [name, value] of Object.entries(viewer)) params.set(name, String(value));
  yield* searchMessages(`/api/title/sources?${params}`, { signal });
}

/**
 * Plays a film or an episode: the engine finds its sources and tries them in order, saying what it
 * does at each step. Throws an EngineError when the engine refuses the request before it starts.
 */
export async function* playTitle(request: PlayTitle, signal?: AbortSignal): AsyncGenerator<PlayMessage> {
  const init = { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(request), signal };
  const response = await fetch("/api/title/play", init);
  if (!response.ok || !response.body) await json<never>(response);
  yield* lines<PlayMessage>(response);
}

/** Kept alive past the page, since the player saves when the page hides. */
export function saveWatched(entry: { video: string; position: number; duration: number | null; release?: string }): Promise<Watched> {
  const init = { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(entry), keepalive: true };
  return fetch("/api/watched", init)
    .then(json<{ watched: Watched }>)
    .then((body) => body.watched);
}

export function setSaved(title: string, saved: boolean): Promise<boolean> {
  return post<{ saved: boolean }>("/api/library", { title, saved }).then((body) => body.saved);
}

export function home(signal?: AbortSignal): Promise<Home> {
  return fetch("/api/home", { signal }).then(json<Home>);
}

export function version(signal?: AbortSignal): Promise<Version> {
  return fetch("/api/version", { signal }).then(json<Version>);
}

export function tvs(signal?: AbortSignal): Promise<TvList> {
  return fetch("/api/tvs", { signal }).then(json<TvList>);
}

/** Searches the home network for TVs, unless a search ran in the last 30 seconds and `fresh` is not set. */
export function findTvs(fresh = false): Promise<TvList> {
  return post("/api/tvs", { fresh });
}

export function startCast(start: CastStart): Promise<Cast> {
  return post<{ cast: Cast }>("/api/cast", start).then((body) => body.cast);
}

export function controlCast(tv: string, action: CastAction): Promise<Cast> {
  return post<{ cast: Cast }>("/api/cast/control", { tv, ...action }).then((body) => body.cast);
}
