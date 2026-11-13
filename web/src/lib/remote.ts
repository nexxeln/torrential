import { useEffect } from "react";
import { goBack, homePath, navigate, type Route } from "../router";
import { isBack, isTv } from "./tv";

export type Direction = "up" | "down" | "left" | "right";

const DIRECTIONS: Partial<Record<string, Direction>> = { ArrowUp: "up", ArrowDown: "down", ArrowLeft: "left", ArrowRight: "right" };

export interface Box {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

const middle = (from: number, to: number) => (from + to) / 2;

/**
 * Which of `boxes` an arrow press in `direction` moves to from `from`, or -1 for none. Left and
 * Right stay in the row. Up and Down go to another row, one that starts higher or lower, so a taller
 * card beside `from` is not above it. They take the nearest box, count a box off to the side as
 * twice as far as its gap, and among boxes in line prefer the one whose left edge lines up, so Down
 * from a wide search field lands on the first card under it.
 */
export function nearest(from: Box, direction: Direction, boxes: Box[]): number {
  const vertical = direction === "up" || direction === "down";
  const sign = direction === "down" || direction === "right" ? 1 : -1;
  let best = -1;
  let bestScore = Infinity;
  boxes.forEach((box, at) => {
    const ahead = vertical
      ? sign * (box.top - from.top) > 1 && sign * (middle(box.top, box.bottom) - middle(from.top, from.bottom)) > 1
      : sign * (middle(box.left, box.right) - middle(from.left, from.right)) > 1 &&
        Math.min(box.bottom, from.bottom) - Math.max(box.top, from.top) > 0;
    if (!ahead) return;
    const gap = vertical
      ? Math.max(0, sign > 0 ? box.top - from.bottom : from.top - box.bottom)
      : Math.max(0, sign > 0 ? box.left - from.right : from.left - box.right);
    const overlap = vertical
      ? Math.min(box.right, from.right) - Math.max(box.left, from.left)
      : Math.min(box.bottom, from.bottom) - Math.max(box.top, from.top);
    const aside = Math.max(0, -overlap);
    const lineUp = vertical ? Math.abs(box.left - from.left) : Math.abs(middle(box.top, box.bottom) - middle(from.top, from.bottom));
    const score = gap + 2 * aside + 0.02 * lineUp;
    if (score < bestScore) {
      best = at;
      bestScore = score;
    }
  });
  return best;
}

const FOCUSABLE = "a[href], button:not(:disabled), input:not(:disabled):not([type=hidden]), select:not(:disabled), textarea:not(:disabled), [tabindex]";
/** Fields that use Left and Right to move the caret or change a value. */
const SIDEWAYS = "input:not([type=radio]):not([type=checkbox]), textarea, [role=slider]";
const TEXT = "input:not([type=radio]):not([type=checkbox]):not([type=range]), textarea";

/** A radio button drawn as its label is where its label is. */
function boxOf(element: Element): DOMRect {
  return (element.matches("input[type=radio], input[type=checkbox]") ? (element.closest("label") ?? element) : element).getBoundingClientRect();
}

function focusables(scope: Element): HTMLElement[] {
  return [...scope.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((element) => {
    if (element.tabIndex < 0 || element.closest("[inert]")) return false;
    const box = boxOf(element);
    return box.width > 0 && box.height > 0 && getComputedStyle(element).visibility !== "hidden";
  });
}

/** Focuses `element`, and on a page scrolls it toward the middle of the screen when it is near an edge. */
function land(element: HTMLElement, page: boolean): void {
  element.focus({ preventScroll: !page });
  if (!page) return;
  const box = element.getBoundingClientRect();
  const margin = innerHeight * 0.15;
  if (box.top < margin || box.bottom > innerHeight - margin) element.scrollIntoView({ block: "center", inline: "nearest" });
}

/** The first thing to focus on a page: what it shows, rather than its search field. */
function first(scope: Element): HTMLElement | null {
  return focusables(scope).find((element) => !element.closest("search")) ?? null;
}

/**
 * Where arrow keys move focus from `active`, or null when they do something else there: an open
 * panel or dialog keeps focus to itself, the player seeks with them until one of its controls has
 * focus, and menus and fields use their own.
 */
function scope(active: HTMLElement | null, direction: Direction): Element | null {
  if (active?.closest("[role=menu]")) return null;
  if ((direction === "left" || direction === "right") && active?.matches(SIDEWAYS)) return null;
  const overlays = document.querySelectorAll("[data-overlay]");
  if (overlays.length) return overlays[overlays.length - 1]!;
  const player = document.querySelector("[data-player]");
  if (player) return active && player.contains(active) ? player : null;
  return document.body;
}

function pressed(event: KeyboardEvent, onBack: (() => void) | null): void {
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  const active = document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null;
  const direction = DIRECTIONS[event.key];
  if (direction) {
    const within = scope(active, direction);
    if (!within) return;
    // The page's own arrow keys, such as the poster grid's, stand down: one rule moves focus everywhere.
    event.stopPropagation();
    const page = within === document.body;
    const candidates = focusables(within).filter((element) => element !== active);
    const target = active
      ? candidates[nearest(boxOf(active), direction, candidates.map(boxOf))]
      : (first(page ? (document.querySelector("main") ?? within) : within) ?? candidates[0]);
    if (!target) return;
    event.preventDefault();
    land(target, page);
  } else if (event.key === "Enter" && active?.matches("input[type=radio], input[type=checkbox]")) {
    // Only the choice hears it, not a list that plays its selected row on Enter.
    event.preventDefault();
    event.stopPropagation();
    active.click();
  } else if (onBack && isBack(event)) {
    // Escape in a search field with text clears it, and the field does that itself.
    if (event.key === "Escape" && active instanceof HTMLInputElement && active.value) return;
    event.preventDefault();
    event.stopPropagation();
    if (active?.matches(TEXT)) active.blur();
    else onBack();
  }
}

/** Puts focus on the first thing a page shows once it is there, so a remote always has a place to start. */
function settle(): () => void {
  const place = () => {
    if (document.activeElement && document.activeElement !== document.body) return true;
    const main = document.querySelector("main");
    const found = main && first(main);
    found?.focus();
    return Boolean(found);
  };
  if (place()) return () => {};
  const watcher = new MutationObserver(() => place() && stop());
  const stop = () => {
    watcher.disconnect();
    clearTimeout(timer);
  };
  // A page with nothing but a search field gets the field.
  const timer = setTimeout(() => {
    stop();
    if (document.activeElement === document.body) document.querySelector<HTMLElement>(`main :is(${TEXT})`)?.focus();
  }, 3000);
  watcher.observe(document.body, { childList: true, subtree: true });
  return stop;
}

/**
 * When the control with focus leaves the page, as a list re-sorts or a stale answer gives way to a
 * new one, focus goes to the control now nearest where it was instead of falling back to the top.
 * A new page is left to `settle`.
 */
function rescue(): () => void {
  let last: { element: Element; href: string; x: number; y: number } | null = null;
  const focused = (event: FocusEvent) => {
    if (!(event.target instanceof Element)) return;
    const box = event.target.getBoundingClientRect();
    last = { element: event.target, href: location.href, x: middle(box.left, box.right), y: middle(box.top, box.bottom) };
  };
  const watcher = new MutationObserver(() => {
    if (!last || last.href !== location.href || last.element.isConnected || document.activeElement !== document.body) return;
    const { x, y } = last;
    const distance = (element: Element) => {
      const box = boxOf(element);
      return Math.hypot(middle(box.left, box.right) - x, middle(box.top, box.bottom) - y);
    };
    const nearby = focusables(document.body).sort((a, b) => distance(a) - distance(b))[0];
    nearby?.focus({ preventScroll: true });
  });
  addEventListener("focusin", focused);
  watcher.observe(document.body, { childList: true, subtree: true });
  return () => {
    removeEventListener("focusin", focused);
    watcher.disconnect();
  };
}

/** What Back does on each page. Null in the player, which answers Back itself, and on the first page. */
function backFrom(route: Route): (() => void) | null {
  switch (route.page) {
    case "play":
    case "watch":
      return null;
    case "home":
      return route.query ? () => navigate(homePath(), { replace: true }) : null;
    default:
      return () => goBack("/");
  }
}

/**
 * On a TV, arrow keys move focus to the nearest control on screen, Enter picks a radio button, and
 * Back leaves the page. Focus settles on each new page, except in the player, which places it
 * itself, and is never lost to a control that leaves the page.
 */
export function useRemote(route: Route): void {
  const player = route.page === "play" || route.page === "watch";
  useEffect(() => {
    if (isTv()) return rescue();
  }, []);

  useEffect(() => {
    if (!isTv()) return;
    const onBack = backFrom(route);
    const listener = (event: KeyboardEvent) => pressed(event, onBack);
    addEventListener("keydown", listener, true);
    return () => removeEventListener("keydown", listener, true);
  }, [route]);

  useEffect(() => {
    if (isTv() && !player) return settle();
  }, [route, player]);
}
