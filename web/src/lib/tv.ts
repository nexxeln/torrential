/** What a browser says about its screen and how it is driven. */
export interface Signals {
  userAgent: string;
  /** The screen's width in CSS pixels. TV browsers report 960, 1280, or 1920. */
  width: number;
  touch: boolean;
  /** No pointing device at all, as a browser driven only by a remote's arrow keys reports. */
  pointerless: boolean;
}

/** The names TV browsers put in their user agent: LG webOS, Samsung Tizen, Sony, Hisense, Google TV. */
const TV_BROWSER = /web0s|webos|tizen|smart-?tv|bravia|netcast|hbbtv|vidaa|googletv|android tv/i;

/** A big screen without touch, whose browser either has no pointer or names a TV platform. */
export function looksLikeTv({ userAgent, width, touch, pointerless }: Signals): boolean {
  return width >= 960 && !touch && (pointerless || TV_BROWSER.test(userAgent));
}

const CHOICE = "torrential:tv";

/** `?tv=on` or `?tv=off` in any address settles the question for this browser, and `?tv=auto` asks the signals again. */
function chosen(): boolean | null {
  const asked = new URLSearchParams(location.search).get("tv");
  try {
    if (asked === "on" || asked === "off") localStorage.setItem(CHOICE, asked);
    if (asked === "auto") localStorage.removeItem(CHOICE);
    const kept = localStorage.getItem(CHOICE);
    if (kept === "on" || kept === "off") return kept === "on";
  } catch {
    // A browser that refuses storage still follows the address it was opened with.
  }
  return asked === "on" ? true : asked === "off" ? false : null;
}

let tv: boolean | null = null;

/**
 * This browser is a TV driven by a remote: arrow keys move focus anywhere on the page, Back goes
 * back, and everything is drawn to be read from across a room.
 */
export function isTv(): boolean {
  tv ??=
    chosen() ??
    looksLikeTv({
      userAgent: navigator.userAgent,
      width: screen.width,
      touch: navigator.maxTouchPoints > 0,
      pointerless: matchMedia("(any-pointer: none)").matches,
    });
  return tv;
}

/** LG webOS sends Back as key code 461, and Samsung Tizen as 10009. */
const BACK_CODES = new Set([461, 10009]);

/** Back on a remote. Escape on any device, and on a TV also Backspace outside a text field. */
export function isBack(event: KeyboardEvent): boolean {
  if (event.key === "Escape" || event.key === "GoBack" || BACK_CODES.has(event.keyCode)) return true;
  return event.key === "Backspace" && isTv() && !(event.target as Element).closest("input, textarea, [contenteditable]");
}
