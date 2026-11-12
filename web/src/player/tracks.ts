import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { type MediaInfo, type OnlineSubtitle, onlineSubtitles, type PlayInfo } from "../api";
import { languageCode, languageInPath, languageLabel, languageName, preferredLanguages } from "../lib/language";
import { fileName } from "../lib/release";
import { preferences, savePreferences } from "../lib/storage";

export type SubtitleSource =
  /** A subtitle file of the torrent. */
  | { kind: "file"; index: number }
  /** A text track inside the video file. */
  | { kind: "embedded"; stream: number }
  /** A subtitle the engine found on a subtitle site. */
  | { kind: "online"; id: string };

export interface SubtitleOption {
  /** Stable for one video file. Names the track in the offset memory. */
  key: string;
  label: string;
  detail: string | null;
  /** ISO 639-1, when the track's language is one the browser can name. */
  language: string | null;
  /** The track's own language tag, which tells regional forms apart. */
  tag: string | null;
  /** Online tracks made for this exact file fit its timing. Tracks of the torrent always do. */
  exact: boolean;
  /** Null for picture subtitles, which cannot be drawn as text. */
  source: SubtitleSource | null;
}

function localOptions(play: PlayInfo, media: MediaInfo): SubtitleOption[] {
  const files = play.subtitles.map((file): SubtitleOption => {
    const language = languageInPath(file.path);
    return {
      key: `file:${file.index}`,
      label: language ? languageName(language) : fileName(file.path),
      detail: language ? fileName(file.path) : "A file in this torrent",
      language,
      tag: language,
      exact: true,
      source: { kind: "file", index: file.index },
    };
  });
  const embedded = media.subtitles.map((track): SubtitleOption => {
    const language = languageCode(track.language);
    const named = track.title && track.title !== (language && languageName(language)) ? track.title : null;
    const notes = track.text
      ? ["In the video file", named, track.forced ? "forced" : null]
      : ["Picture subtitles, not shown here"];
    return {
      key: `embedded:${track.stream}`,
      label: language ? languageName(language) : (track.title ?? `Track ${track.stream}`),
      detail: notes.filter(Boolean).join(" · "),
      language,
      tag: track.language,
      exact: true,
      source: track.text ? { kind: "embedded", stream: track.stream } : null,
    };
  });
  return [...files, ...embedded];
}

function onlineOption(subtitle: OnlineSubtitle): SubtitleOption {
  return {
    key: `online:${subtitle.id}`,
    label: languageLabel(subtitle.language),
    detail: subtitle.name,
    language: languageCode(subtitle.language),
    tag: subtitle.language,
    exact: subtitle.exact,
    source: { kind: "online", id: subtitle.id },
  };
}

/** What the online lookup has found so far. */
export type OnlineLookup =
  | { kind: "looking" }
  /** Best first within each language, exact matches before the rest. */
  | { kind: "found"; options: SubtitleOption[] }
  | { kind: "nothing" }
  | { kind: "failed" };

function useOnlineLookup(id: string, file: number): OnlineLookup {
  const [lookup, setLookup] = useState<OnlineLookup>({ kind: "looking" });
  useEffect(() => {
    const stop = new AbortController();
    onlineSubtitles(id, file, stop.signal)
      .then((found) => {
        const options = found.subtitles.map(onlineOption);
        // A stable sort keeps the engine's order within a language and within each kind of match.
        options.sort((a, b) => Number(b.exact) - Number(a.exact));
        setLookup(options.length ? { kind: "found", options } : { kind: "nothing" });
      })
      .catch(() => {
        if (!stop.signal.aborted) setLookup({ kind: "failed" });
      });
    return () => stop.abort();
  }, [id, file]);
  return lookup;
}

function bestIn(options: SubtitleOption[], languages: string[]): SubtitleOption | null {
  for (const language of languages) {
    const match = options.find((option) => option.source && option.language === language);
    if (match) return match;
  }
  return null;
}

export interface Subtitles {
  /** Tracks of the torrent: its subtitle files, then the video's own tracks. */
  local: SubtitleOption[];
  online: OnlineLookup;
  /** The track showing, or null for none. */
  chosen: SubtitleOption | null;
  /** The person picks a track, or turns subtitles off with null. Remembers the language. */
  choose: (key: string | null) => void;
  /**
   * Turns subtitles off, or on: the track last shown, else one in a preferred language from the
   * torrent, else the best online one. Returns what is showing after, or `"none"` when there is
   * nothing to turn on.
   */
  toggle: () => SubtitleOption | null | "none";
}

/**
 * The subtitle tracks of one video and which one shows. The online lookup runs beside playback
 * and never holds it up. When the person has subtitles on in a language the torrent lacks, the
 * best online track in that language turns on by itself once it is found.
 */
export function useSubtitleTracks(play: PlayInfo, media: MediaInfo): Subtitles {
  const local = useMemo(() => localOptions(play, media), [play, media]);
  const online = useOnlineLookup(play.id, play.video.index);
  const found = online.kind === "found" ? online.options : null;
  const all = useMemo(() => [...local, ...(found ?? [])], [local, found]);

  const wanted = useRef(preferences().subtitles);
  const [key, setKey] = useState<string | null>(() => bestIn(local, wanted.current ? [wanted.current] : [])?.key ?? null);
  /** The person has chosen in this session, so nothing turns on by itself any more. */
  const decided = useRef(false);
  const last = useRef(key);

  useEffect(() => {
    if (decided.current || key !== null || !found || !wanted.current) return;
    const match = bestIn(found, [wanted.current]);
    if (match) {
      last.current = match.key;
      setKey(match.key);
    }
  }, [found, key]);

  const choose = useCallback(
    (next: string | null) => {
      decided.current = true;
      if (next) last.current = next;
      setKey(next);
      const language = all.find((option) => option.key === next)?.language;
      if (next === null || language) {
        wanted.current = language ?? null;
        savePreferences({ subtitles: wanted.current });
      }
    },
    [all],
  );

  const toggle = useCallback((): SubtitleOption | null | "none" => {
    if (key) {
      choose(null);
      return null;
    }
    const languages = preferredLanguages(wanted.current);
    const again = all.find((option) => option.key === last.current && option.source);
    const pick = again ?? bestIn(local, languages) ?? bestIn(found ?? [], languages) ?? local.find((option) => option.source);
    if (!pick) return "none";
    choose(pick.key);
    return pick;
  }, [key, all, local, found, choose]);

  const chosen = all.find((option) => option.key === key) ?? null;
  return { local, online, chosen, choose, toggle };
}
