import type { Release } from "../api";

const VIDEO_FILE = /\.(mkv|mp4|m4v|mov|webm|avi|ts|m2ts|wmv|mpg|mpeg|ogv)$/i;
/** Where a release name stops being the title: a year, an episode, or a resolution. */
const TAGS = /(?<=[\s.(\[-])(?:(?:19|20)\d{2}(?=[\s.)\]]|$)|S\d{1,2}(?:E\d{1,3})?\b|\d{3,4}p\b|4k\b|uhd\b|complete\b)/i;
const CLOSING: Record<string, string> = { "(": ")", "[": "]" };

export interface ReadableName {
  /** The words before the first tag: `Night of the Living Dead`. */
  title: string;
  /** The tags that follow: `1968 1080p BluRay x264-GROUP`. Empty when the name has none. */
  tags: string;
}

/** Splits a release or file name into the title a person reads and the tags that describe the encode. */
export function readable(name: string): ReadableName {
  const base = name.replace(VIDEO_FILE, "");
  // Scene names use dots or underscores for spaces. A name that already has spaces keeps its punctuation.
  const spaced = /\s/.test(base) ? base : base.replace(/[._]+/g, " ");
  const cut = TAGS.exec(spaced);
  if (!cut || cut.index === 0) return { title: spaced.trim(), tags: "" };
  const before = spaced.slice(0, cut.index);
  const title = before.replace(/[\s.([\-]+$/, "");
  // A year in brackets, as in `Sintel (2010)`, opens in the title and closes in the tags.
  const closing = CLOSING[before.trimEnd().at(-1) ?? ""];
  const after = spaced.slice(cut.index);
  const tags = (closing ? after.replace(closing, "") : after).replace(/^[\s)\]]+|[\s([]+$/g, "");
  return title ? { title, tags } : { title: spaced.trim(), tags: "" };
}

/** A release the player can try: tagged as video, or untagged but named like one. */
export function isVideo(release: Release): boolean {
  if (release.category) return release.category === "video";
  return release.quality.resolution !== null || release.quality.codec !== null;
}

export function isVideoFile(path: string): boolean {
  return VIDEO_FILE.test(path) && !path.split("/").some((part) => /^sample/i.test(part));
}

export function fileName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}
