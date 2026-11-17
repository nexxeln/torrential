// The engine's JSON, as the Rust types that write it describe it. `cargo test` writes this file
// from crates/core and crates/daemon/src/daemon/web, and fails when it was out of date.

export type ErrorCode = "usage" | "confirmation_required" | "invalid_ref" | "invalid_torrent" | "not_found" | "ambiguous_ref" | "job_conflict" | "path_conflict" | "providers_unavailable" | "metadata_timeout" | "network_unavailable" | "fetch_failed" | "sandbox_network_blocked" | "sandbox_write_blocked" | "daemon_unavailable" | "daemon_version" | "state_corrupt" | "disk_full" | "too_large" | "no_selection" | "storage_error" | "verification_failed" | "no_source" | "interrupted" | "not_paired" | "local_only" | "internal";

export type Details = { "candidates": Array<Candidate> } | { "sources": Array<SourceReport> } | { "space": { required: number, available: number, } } | { "sandbox": { signal: string, harness: string | null, } } | { "versions": { client: string, daemon: string, } } | { "paths": Array<string> } | { "tried": Array<Tried> };

export type Candidate = { id: string, name: string, kind: CandidateKind, };

export type CandidateKind = "download" | "search result";

export type Error = { code: ErrorCode, message: string, hint?: string | null, details?: Details | null, next?: string | null, };

export type Category = "video" | "audio" | "software" | "book" | "game" | "dataset" | "other";

export type Flag = "official" | "trusted" | "public_domain" | "creative_commons" | "cam" | "adult";

export type Resolution = "sd" | "720p" | "1080p" | "2160p";

export type Codec = "h264" | "hevc" | "av1";

export type Quality = {
/**
 * `None` when the name carries no resolution tag.
 */
resolution: Resolution | null, codec: Codec | null,
/**
 * HDR10, HDR10+, or Dolby Vision. False when the name does not say, which
 * usually means SDR.
 */
hdr: boolean, };

export type Swarm = "strong" | "fair" | "weak" | "unknown";

export type Release = { id: string, name: string, size: number | null, seeders: number | null, leechers: number | null, published: number | null, category: Category | null, quality: Quality, flags: Array<Flag>, sources: Array<string>, magnet: string | null, torrent_url: string | null, info_url: string | null, span?: string, swarm?: Swarm, failed?: Skip, };

export type SourceOutcome = { "outcome": "ok", results: number, } | { "outcome": "cached", results: number, } | { "outcome": "timeout" } | { "outcome": "http", status: number, } | { "outcome": "connect", reason: string, } | { "outcome": "bad_response", reason: string, } | { "outcome": "irrelevant" };

export type SourceReport = { source: string, elapsed_ms: number, } & ({ "outcome": "ok", results: number, } | { "outcome": "cached", results: number, } | { "outcome": "timeout" } | { "outcome": "http", status: number, } | { "outcome": "connect", reason: string, } | { "outcome": "bad_response", reason: string, } | { "outcome": "irrelevant" });

export type SearchResult = {
/**
 * The web app does not read it, so its TypeScript does not describe it.
 */
query: unknown,
/**
 * Ranked, filtered, and cut to `query.limit`.
 */
releases: Array<Release>,
/**
 * Matching releases before the limit.
 */
total: number,
/**
 * Hidden by the adult filter.
 */
hidden_adult: number, sources: Array<SourceReport>, elapsed_ms: number, };

export type Place = "folder" | "plays";

export type Selection = { "kind": "all" } | { "kind": "patterns", "value": Array<string> };

export type JobStatus = { "state": "queued" } | { "state": "metadata" } | { "state": "checking" } | { "state": "downloading" } | { "state": "paused" } | { "state": "done", finished_at: number, } | { "state": "error", error: Error, };

export type Progress = {
/**
 * Bytes selected for download. `None` until metadata arrives.
 */
size: number | null,
/**
 * Selected bytes in pieces that passed their hash check.
 */
verified: number, piece_map?: string | null,
/**
 * Payload bytes per second.
 */
down_rate: number, up_rate: number, peers: number, seeds: number,
/**
 * Seconds to completion at the current rate. `None` until the rate has stayed
 * above zero for ten seconds.
 */
eta: number | null, };

export type TitleLink = { title: string, span: string,
/**
 * The search result the download came from.
 */
release: string | null, };

export type Job = { id: string, info_hash: string,
/**
 * Unknown until metadata arrives for a bare infohash.
 */
name: string | null, dir: string,
/**
 * `dir` joined with the torrent's name, once the name is known.
 */
path: string | null, progress: Progress, selection: Selection,
/**
 * Unix seconds.
 */
added_at: number,
/**
 * The title the download was made for, when playing a title made it.
 */
title?: TitleLink | null, place: Place,
/**
 * The network interface this download reaches peers through, named while it is missing or
 * down. Nothing arrives until it is back.
 */
waiting_for?: string | null, } & ({ "state": "queued" } | { "state": "metadata" } | { "state": "checking" } | { "state": "downloading" } | { "state": "paused" } | { "state": "done", finished_at: number, } | { "state": "error", error: Error, });

export type Event = { "event": "snapshot", jobs: Array<Job>, } | { "event": "job", job: Job, } | { "event": "removed", id: string, } | { "event": "heartbeat", at: number, } | { "event": "search_update", result: SearchResult, pending: Array<string>, };

export type FileRisk = "executable";

export type TorrentFile = { index: number,
/**
 * Relative to the torrent's folder, sanitized.
 */
path: string, size: number, selected: boolean, verified: number, risk?: FileRisk | null, };

export type MetadataSource = "download" | "cache" | "torrent_file" | "torrent_url" | "peers";

export type FilesReply = { id: string, name: string | null, total_size: number, files: Array<TorrentFile>, metadata_source: MetadataSource, elapsed_ms: number, };

export type ChangeResult = "paused" | "already_paused" | "already_done" | "already_stopped" | "resumed" | "already_running" | "removed" | "already_removed" | "checking";

export type Kept = { path: string, reason: string, };

export type Change = { id: string, name: string | null, result: ChangeResult, path?: string | null,
/**
 * What `rm` deleted from disk: a play's folder.
 */
deleted?: Array<string>,
/**
 * What `rm --delete` moved to the Trash: the files of a download in a folder the person chose.
 */
trashed?: Array<string>,
/**
 * What `rm` was to delete and left.
 */
kept?: Array<Kept>, };

export type Kind = "film" | "series";

export type Country = "US" | "UK" | "AU" | "CA" | "NZ" | "IE";

export type TitleCard = { id: string, kind: Kind, name: string,
/**
 * A film's year, or the year a series began.
 */
year: number | null,
/**
 * IMDb's rating, 0 to 10, written with the one decimal IMDb shows.
 */
rating: number | null, };

export type Art = { poster: boolean, background: boolean, logo: boolean, };

export type EpisodeInfo = { name: string | null,
/**
 * Unix seconds. `None` when no date is announced.
 */
aired: number | null,
/**
 * The air date has passed, so the episode plays.
 */
has_aired: boolean, overview: string | null, thumbnail: boolean, season: number, episode: number, };

export type Season = { number: number, episodes: Array<EpisodeInfo>, };

export type TitleDetails = { synopsis: string | null, genres: Array<string>, runtime_min: number | null, cast: Array<string>,
/**
 * The countries the title is from, of those whose codes release names carry. Empty when it
 * is from none of them or the catalog does not say.
 */
countries: Array<Country>, art: Art,
/**
 * Empty for a film. Season 0 holds specials.
 */
seasons: Array<Season>, id: string, kind: Kind, name: string,
/**
 * A film's year, or the year a series began.
 */
year: number | null,
/**
 * IMDb's rating, 0 to 10, written with the one decimal IMDb shows.
 */
rating: number | null, };

export type Watched = { video: string, position: number, duration: number | null, finished: boolean, at: number, };

export type Next = { video: string,
/**
 * Seconds to resume at, or `None` to start from the beginning.
 */
resume_at: number | null, };

export type TitlePage = { saved: boolean,
/**
 * Every film or episode of the title with a position, the most recently watched last.
 */
watched: Array<Watched>,
/**
 * `None` when the film is finished or the series is caught up.
 */
next: Next | null,
/**
 * What Play plays: `next`, or else the film or the first aired episode from the start. `None`
 * while no episode of a series has aired.
 */
play_target: Next | null,
/**
 * The downloads made by playing this title.
 */
downloads: Array<Job>, synopsis: string | null, genres: Array<string>, runtime_min: number | null, cast: Array<string>,
/**
 * The countries the title is from, of those whose codes release names carry. Empty when it
 * is from none of them or the catalog does not say.
 */
countries: Array<Country>, art: Art,
/**
 * Empty for a film. Season 0 holds specials.
 */
seasons: Array<Season>, id: string, kind: Kind, name: string,
/**
 * A film's year, or the year a series began.
 */
year: number | null,
/**
 * IMDb's rating, 0 to 10, written with the one decimal IMDb shows.
 */
rating: number | null, };

export type Continue = { title: TitleCard, video: string, episode: EpisodeInfo | null,
/**
 * `None` when the video has not been started, as the episode after a finished one.
 */
resume_at: number | null, duration: number | null, };

export type Home = {
/**
 * The most recently watched first.
 */
continue: Array<Continue>, saved: Array<TitleCard>, };

export type Check = "file_list" | "start" | "first_bytes";

export type Skip = { "reason": "no_file_list", waited_s: number, text: string, } | { "reason": "no_video", text: string, } | { "reason": "unplayable", container: string, text: string, } | { "reason": "not_on_tv", container: string, text: string, } | { "reason": "too_large", error: Error, text: string, } | { "reason": "no_data", waited_s: number, text: string, } | { "reason": "stalled", text: string, } | { "reason": "failed", error: Error, text: string, };

export type Tried = { release: string, reason: Skip, };

export type AttemptEvent = { "type": "resolved", video: string, title: TitleCard, episode: EpisodeInfo | null, } | { "type": "searched", found: number, sources: Array<SourceReport>, } | { "type": "trying", rank: number, of: number, release: Release, } | { "type": "check", check: Check, } | { "type": "skipped", release: string, reason: Skip, } | { "type": "playing", video: string, release: Release, job: string, file: number, resume_at: number | null, } | { "type": "failed", error: Error, tried: Array<Tried>, };

export type Playback = "hevc-hdr" | "hevc" | "h264";

export type Decoding = "hardware" | "software" | "no";

export type Decoders = { h264: Decoding, hevc: Decoding, av1: Decoding, };

export type Device = {
/**
 * Eight hex digits, which `torrential devices remove` takes.
 */
id: string,
/**
 * What the browser said it is, such as "iPhone" or "LG TV".
 */
name: string,
/**
 * Unix seconds.
 */
added_at: number,
/**
 * Unix seconds of its last request, to within a minute.
 */
seen_at: number,
/**
 * The address its last request came from, such as `192.168.1.40` or `100.101.3.4`. `None`
 * for a device paired before torrential 0.7 that has not asked since.
 */
seen_from: string | null, };

export type Container = "mkv" | "mp4" | "webm" | "avi" | "ts" | "mov" | "mpeg" | "wmv";

export type Tv = {
/**
 * The renderer's UDN and its address, such as `uuid:6a3e…@192.168.1.40`. A device that
 * copies a TV's UDN from another address is another TV, and so is the TV at a new address.
 */
id: string,
/**
 * The name the TV gives itself, such as `[LG] webOS TV OLED55C1`.
 */
name: string,
/**
 * Its IP address on the home network.
 */
address: string,
/**
 * The containers a cast sends it as they are.
 */
plays: Array<Container>,
/**
 * The newest video codec it says it decodes, which picks the sources a title cast tries.
 */
decodes: Playback,
/**
 * Whether torrential can set its volume.
 */
volume: boolean, };

export type CastState = { "state": "finding" } | { "state": "loading" } | { "state": "playing" } | { "state": "paused" } | { "state": "ended", error: Error | null, };

export type Cast = {
/**
 * The TV's ID.
 */
tv: string, tv_name: string,
/**
 * The film's name and year, the series' name and the episode, or the file's name.
 */
name: string,
/**
 * The film or episode, whose position the watch history keeps.
 */
video: string | null,
/**
 * The download it plays from, once a source starts.
 */
download: string | null, file: number | null,
/**
 * The subtitle file sent with the video, relative to the torrent's folder.
 */
subtitle: string | null,
/**
 * Seconds, as the TV last said.
 */
position: number | null, duration: number | null,
/**
 * From 0 to 100, for a TV whose volume torrential can set.
 */
volume: number | null,
/**
 * Unix seconds.
 */
started_at: number, } & ({ "state": "finding" } | { "state": "loading" } | { "state": "playing" } | { "state": "paused" } | { "state": "ended", error: Error | null, });

export type TvList = { tvs: Array<Tv>, casts: Array<Cast>, };

export type CastAction = { "action": "pause" } | { "action": "resume" } | { "action": "seek", position: number, } | { "action": "stop" } | { "action": "volume", level: number, };

export type StreamedFile = { index: number, path: string, size: number,
/**
 * The path of `http::url`, for a page on the engine's own origin.
 */
url: string,
/**
 * Where the file starts in the torrent, in bytes.
 */
offset: number, };

export type PlayInfo = { id: string, name: string, job: Job, files: Array<TorrentFile>, total_size: number, video: StreamedFile, subtitles: Array<StreamedFile>, };

export type Readiness = "ready" | "fetch" | "missing";

export type VideoTrack = { stream: number, codec: string,
/**
 * The RFC 6381 codec string. `None` when MP4 cannot carry the codec.
 */
mime: string | null, width: number, height: number, bit_depth: number, hdr: boolean, fps: number | null, };

export type AudioTrack = { stream: number, codec: string, channels: number, language: string | null, title: string | null, default: boolean, };

export type SubtitleTrack = { stream: number, codec: string, language: string | null, title: string | null, default: boolean, forced: boolean,
/**
 * False for picture subtitles, which cannot become WebVTT.
 */
text: boolean, };

export type Chapter = {
/**
 * Seconds.
 */
start: number, end: number, title: string | null, };

export type MediaInfo = {
/**
 * Seconds.
 */
duration: number | null, container: string, video: VideoTrack | null, audio: Array<AudioTrack>, subtitles: Array<SubtitleTrack>,
/**
 * A type for `canPlayType`, when browsers read the file's container.
 */
direct: string | null,
/**
 * Whether the engine can convert the file as it plays.
 */
ffmpeg: Readiness,
/**
 * The file's own chapters, in order.
 */
chapters: Array<Chapter>, };

export type OnlineTitle = { name: string, year: number | null, season: number | null, episode: number | null, };

export type OnlineSubtitle = { id: string, language: string, name: string | null, exact: boolean, source: string, };

export type OnlineSubtitles = {
/**
 * What the names in the torrent say the video is.
 */
title: OnlineTitle | null, subtitles: Array<OnlineSubtitle>,
/**
 * The search had the file's hash. Until it does, asking again once both ends of the file have
 * downloaded can find subtitles timed for this exact file.
 */
hashed: boolean, };

export type FilePieces = { pieces: string, };

export type Version = { version: string,
/**
 * What the update board says, if anything.
 */
notice: string | null, };

export type SearchMessage = { "type": "update", result: SearchResult, pending: Array<string>, } | { "type": "done", result: SearchResult, } | { "type": "error", error: Error, };

export type FfmpegMessage = { "type": "progress", done: number, size: number, } | { "type": "ready" } | { "type": "error", error: Error, };

export type TitleSearch = { titles: Array<TitleCard>,
/**
 * Title lookups are off in config.yaml.
 */
offline?: boolean, };

export type Viewer = {
/**
 * The screen in device pixels.
 */
width: number, height: number,
/**
 * Device pixels to a CSS pixel.
 */
scale: number, playback: Playback,
/**
 * How the browser decodes each codec, from its `mediaCapabilities`.
 */
decoders?: Decoders, };

export type PlayTitle = {
/**
 * `tt0063350`, `tt0903747:3:5`, or a series for its next episode.
 */
video: string,
/**
 * The screen that pressed play and what its browser decodes.
 */
viewer?: Viewer,
/**
 * What the browser decodes, from a tab opened before browsers reported their screen.
 */
playback?: Playback,
/**
 * Play only this release.
 */
source?: string,
/**
 * The download that stalled.
 */
replace?: string, };

export type PlayingLine = { "type": "playing", video: string, release: Release, play: PlayInfo, resume_at: number | null, };

export type Position = { video: string,
/**
 * Seconds.
 */
position: number, duration: number | null, release: string | null,
/**
 * A word the tab made up for the video it plays. A tab that sends none is one tab.
 */
tab: string,
/**
 * The tab started or resumed the video since its last save, so it keeps the place.
 */
starts: boolean, };

export type Saved = { watched: Watched,
/**
 * False while another screen keeps the place.
 */
kept: boolean, };

export type Forget = { video: string, };

export type PairingOffer = {
/**
 * Rows of `1` and `0`, without the quiet zone.
 */
qr: Array<string>,
/**
 * Six digits.
 */
code: string,
/**
 * The page that pairs a phone, with the code in it. The QR code holds this.
 */
url: string,
/**
 * What to type into a TV's browser, such as `http://studio-mac.local:47919`: the
 * `public_address` in config.yaml, else this machine's .local name and the port.
 */
address: string,
/**
 * This machine's address on the home network and the port, such as
 * `http://192.168.1.23:47919`, for a browser that cannot open .local names. `None` when the
 * machine has no such address.
 */
lan_address: string | null,
/**
 * This machine's name on its Tailscale network and the port, such as
 * `http://box.tail1234.ts.net:47919`, which a device on the tailnet opens from anywhere.
 * `None` while Tailscale does not run.
 */
tailnet_address: string | null,
/**
 * Unix seconds.
 */
expires_at: number, };

export type DeviceList = { devices: Array<Device>, offer: PairingOffer | null, unreachable: string | null, };

export type Me = { "asker": "device", device: Device | null, } | { "asker": "mac" };

export type CastStart = {
/**
 * A TV's ID. Without it, the only TV on the network.
 */
tv?: string,
/**
 * A film or an episode to play, or with `ref`, the one the torrent holds.
 */
video?: string,
/**
 * For a film or an episode, the one release to try.
 */
source?: string,
/**
 * A download or search result ID, an infohash, a magnet link, or the URL of a .torrent file.
 */
ref?: string, file?: number, subtitle?: number, start_at?: number, };

/** Hex digits every screen shows of an ID. */
export const SHORT_ID = 6;

/** Equal runs of a torrent's pieces in `Progress.piece_map`. */
export const SLICES = 64;

/** Equal runs of a file's bytes in `GET /api/pieces`. */
export const PIECE_RUNS = 200;

/** Results `POST /api/search` answers when it names no limit. */
export const SEARCH_LIMIT = 50;

/** How long a pairing code works. */
export const PAIRING_MINUTES = 10;

/** Seconds of video one thumbnail stands for. */
export const THUMBNAIL_EVERY = 10;

/** Bytes a thumbnail needs on disk each side of its frame. */
export const THUMBNAIL_MARGIN_BYTES = 8388608;

/** The share of a large file a thumbnail needs on disk each side of its frame. */
export const THUMBNAIL_MARGIN_SHARE = 0.02;

/** The share of a video at its end that counts as credits. */
export const FINISHED_SHARE = 0.03;

/** Seconds at the end of a video that count as credits. */
export const FINISHED_WITHIN = 20;

/** Seconds at the start of a video too few to keep a place at. */
export const WORTH_SAVING = 5;

/** How often a player saves where it is. */
export const SAVE_EVERY_MS = 5000;

