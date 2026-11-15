// A film or one episode of a series, written as the engine writes it: `tt0063350`, or
// `tt0903747:3:5` for season 3, episode 5.

/** An IMDb ID, such as `tt0063350`. */
export const TITLE_ID = /^tt\d+$/;

/** A film, or one episode of a series. */
export const VIDEO_REF = /^tt\d+(:\d+:\d+)?$/;

/** The video of a film, or of one episode of a series. */
export function videoOf(id: string, episode?: { season: number; episode: number } | null): string {
  return episode ? `${id}:${episode.season}:${episode.episode}` : id;
}

/** The film or series a video belongs to. */
export function titleOf(video: string): string {
  return video.split(":")[0]!;
}

/** The season and episode a video names, or null for a film. */
export function episodeOf(video: string): { season: number; episode: number } | null {
  const [, season, episode] = video.split(":").map(Number);
  return season === undefined || episode === undefined ? null : { season, episode };
}
