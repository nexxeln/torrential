import type { Check, EpisodeInfo, Skip, TitleCard, TitlePage } from "../api";
import { sentence } from "../lib/format";
import { episodeOf } from "../lib/video";

/** A film's year, or a series' first year and that it is one. */
export function cardFacts(card: TitleCard): string {
  return [card.year, card.kind === "series" ? "Series" : null].filter(Boolean).join(" · ");
}

/** `S3 E5`. */
export function episodeCode(episode: { season: number; episode: number }): string {
  return `S${episode.season} E${episode.episode}`;
}

export function episodeName(episode: EpisodeInfo): string {
  return episode.name ?? `Episode ${episode.episode}`;
}

export function seasonName(number: number): string {
  return number === 0 ? "Specials" : `Season ${number}`;
}

export function findEpisode(page: Pick<TitlePage, "seasons">, video: string): EpisodeInfo | null {
  const wanted = episodeOf(video);
  if (!wanted) return null;
  return page.seasons.find((s) => s.number === wanted.season)?.episodes.find((e) => e.episode === wanted.episode) ?? null;
}

/**
 * The episodes that play one after another: the aired episodes of the regular seasons, or of the
 * specials for a special.
 */
export function playingOrder(page: Pick<TitlePage, "seasons">, specials: boolean): EpisodeInfo[] {
  return page.seasons
    .filter((season) => (season.number === 0) === specials)
    .flatMap((season) => season.episodes)
    .filter((episode) => episode.has_aired);
}

/** What the engine is checking about the source it tries. */
export const CHECKS: Record<Check, string> = {
  file_list: "Asking peers for its list of files.",
  start: "Starting the download.",
  first_bytes: "Waiting for the first pieces of video.",
};

/** Why a source was passed over, as a sentence. */
export function skipText(skip: Skip): string {
  return sentence(skip.text);
}
