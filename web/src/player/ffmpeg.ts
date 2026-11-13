import { EngineError, fetchFfmpeg, type MediaInfo, media } from "../api";
import { plan } from "./plan";

/** How far the engine's ffmpeg download is, in bytes. */
export interface Fetching {
  done: number;
  size: number;
}

/**
 * A file's tracks. When the browser cannot read the file and the engine has no ffmpeg yet, the
 * engine fetches it first, and `onFetch` hears how far the download is.
 */
export async function readMedia(id: string, file: number, signal: AbortSignal, onFetch: (fetching: Fetching) => void): Promise<MediaInfo> {
  const first = await media(id, file, signal);
  const how = plan(first, null, true);
  if (first.ffmpeg !== "fetch" || how.kind !== "blocked" || how.reason !== "ffmpeg") return first;
  let ready = false;
  for await (const message of fetchFfmpeg(signal)) {
    if (message.type === "progress") onFetch({ done: message.done, size: message.size });
    else if (message.type === "error") throw new EngineError(message.error, 200);
    else ready = true;
  }
  // asApiError reads a plain error as an engine that stopped answering, which a stream cut short is.
  if (!ready) throw new Error("the ffmpeg fetch was cut off");
  return media(id, file, signal);
}
