import { type ComponentProps, type MouseEvent, useSyncExternalStore } from "react";

export type Route =
  /** Continue watching and the library, or with a query, the films and shows named like it. */
  | { page: "home"; query: string }
  /** `season` is the season shown, and `sources` the film or episode whose sources are listed. */
  | { page: "title"; id: string; season: number | null; sources: string | null }
  /** A film or an episode, or a series for its next episode. `source` pins one release. */
  | { page: "play"; video: string; source: string | null }
  | { page: "torrents"; query: string; everything: boolean }
  | { page: "watch"; ref: string; file: number | null }
  | { page: "downloads" }
  /** The phones and TVs that watch from this Mac, and the code that adds one. */
  | { page: "devices" }
  | { page: "missing" };

const CHANGED = "torrential:navigate";
const TITLE = /^tt\d+$/;
const VIDEO = /^tt\d+(:\d+:\d+)?$/;

/** A path segment as typed. A malformed escape stays as it is, and names no page. */
function decoded(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

function number(text: string | null): number | null {
  return text === null || !/^\d+$/.test(text) ? null : Number(text);
}

function parse(href: string): Route {
  const url = new URL(href, location.origin);
  const parts = url.pathname.split("/").filter(Boolean).map(decoded);
  const query = url.searchParams;
  if (parts.length === 0) return { page: "home", query: query.get("q") ?? "" };
  if (parts[0] === "title" && parts.length === 2 && TITLE.test(parts[1]!)) {
    const sources = query.get("sources");
    return { page: "title", id: parts[1]!, season: number(query.get("season")), sources: sources && VIDEO.test(sources) ? sources : null };
  }
  if (parts[0] === "play" && parts.length === 2 && VIDEO.test(parts[1]!)) {
    return { page: "play", video: parts[1]!, source: query.get("source") };
  }
  if (parts[0] === "torrents" && parts.length === 1) {
    return { page: "torrents", query: query.get("q") ?? "", everything: query.has("all") };
  }
  if (parts[0] === "watch" && parts.length === 2) {
    const file = query.get("file");
    return { page: "watch", ref: parts[1]!, file: file === null ? null : Number(file) };
  }
  if (parts[0] === "downloads" && parts.length === 1) return { page: "downloads" };
  if (parts[0] === "devices" && parts.length === 1) return { page: "devices" };
  return { page: "missing" };
}

function withQuery(path: string, params: URLSearchParams): string {
  const text = params.toString();
  return text ? `${path}?${text}` : path;
}

export function homePath(query = ""): string {
  return withQuery("/", new URLSearchParams(query ? { q: query } : {}));
}

export function titlePath(id: string, options: { season?: number; sources?: string } = {}): string {
  const params = new URLSearchParams();
  if (options.season !== undefined) params.set("season", String(options.season));
  if (options.sources) params.set("sources", options.sources);
  return withQuery(`/title/${id}`, params);
}

/** `video` keeps its colons, so an episode reads as `/play/tt0903747:3:5`. */
export function playPath(video: string, source?: string): string {
  return withQuery(`/play/${video}`, new URLSearchParams(source ? { source } : {}));
}

export function torrentsPath(query: string, everything: boolean): string {
  const params = new URLSearchParams();
  if (query) params.set("q", query);
  if (everything) params.set("all", "1");
  return withQuery("/torrents", params);
}

export function watchPath(ref: string, file?: number): string {
  return `/watch/${encodeURIComponent(ref)}${file === undefined ? "" : `?file=${file}`}`;
}

/** Pages reached through `navigate` carry this state, so "back" knows the previous entry is ours. */
const IN_APP = { inApp: true };

export function navigate(to: string, options: { replace?: boolean } = {}): void {
  if (options.replace) history.replaceState(history.state, "", to);
  else history.pushState(IN_APP, "", to);
  dispatchEvent(new Event(CHANGED));
}

/** Returns to the page that led here, or to `fallback` when this page was opened directly. */
export function goBack(fallback: string): void {
  if (history.state?.inApp) history.back();
  else navigate(fallback, { replace: true });
}

function subscribe(onChange: () => void): () => void {
  addEventListener("popstate", onChange);
  addEventListener(CHANGED, onChange);
  return () => {
    removeEventListener("popstate", onChange);
    removeEventListener(CHANGED, onChange);
  };
}

let cached: { href: string; route: Route } | null = null;

function current(): Route {
  if (cached?.href !== location.href) cached = { href: location.href, route: parse(location.href) };
  return cached.route;
}

export function useRoute(): Route {
  return useSyncExternalStore(subscribe, current);
}

interface LinkProps extends ComponentProps<"a"> {
  href: string;
  /** Takes the place of the current history entry, so "back" skips it. */
  replace?: boolean;
}

/** A same-origin link that changes the route without loading the page. Modified clicks keep their browser meaning. */
export function Link({ href, replace, onClick, ...rest }: LinkProps) {
  const follow = (event: MouseEvent<HTMLAnchorElement>) => {
    onClick?.(event);
    if (event.defaultPrevented || event.button !== 0) return;
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    navigate(href, { replace });
  };
  return <a href={href} onClick={follow} {...rest} />;
}
