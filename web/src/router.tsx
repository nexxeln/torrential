import { type ComponentProps, type MouseEvent, useSyncExternalStore } from "react";

export type Route =
  | { page: "search"; query: string; everything: boolean }
  | { page: "watch"; ref: string; file: number | null }
  | { page: "downloads" }
  | { page: "missing" };

const CHANGED = "torrential:navigate";

function parse(href: string): Route {
  const url = new URL(href, location.origin);
  const parts = url.pathname.split("/").filter(Boolean);
  if (parts.length === 0) {
    return { page: "search", query: url.searchParams.get("q") ?? "", everything: url.searchParams.has("all") };
  }
  if (parts[0] === "watch" && parts.length === 2) {
    const file = url.searchParams.get("file");
    return { page: "watch", ref: decodeURIComponent(parts[1]!), file: file === null ? null : Number(file) };
  }
  if (parts[0] === "downloads" && parts.length === 1) return { page: "downloads" };
  return { page: "missing" };
}

export function searchPath(query: string, everything: boolean): string {
  const params = new URLSearchParams();
  if (query) params.set("q", query);
  if (everything) params.set("all", "1");
  const text = params.toString();
  return text ? `/?${text}` : "/";
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
