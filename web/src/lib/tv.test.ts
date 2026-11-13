import { expect, test } from "bun:test";
import { looksLikeTv } from "./tv";

const LG = "Mozilla/5.0 (Web0S; Linux/SmartTV) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/108.0.0.0 Safari/537.36";
const SAMSUNG = "Mozilla/5.0 (SMART-TV; LINUX; Tizen 8.0) AppleWebKit/537.36 (KHTML, like Gecko) 108.0.5359.1/8.0 TV Safari/537.36";
const MAC = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
const IPHONE = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Mobile/15E148 Safari/604.1";

test("the LG and Samsung TV browsers on a 1920 screen are TVs", () => {
  expect(looksLikeTv({ userAgent: LG, width: 1920, touch: false, pointerless: false })).toBe(true);
  expect(looksLikeTv({ userAgent: SAMSUNG, width: 1920, touch: false, pointerless: false })).toBe(true);
});

test("a TV browser that reports a 960 pixel wide screen is a TV", () => {
  expect(looksLikeTv({ userAgent: LG, width: 960, touch: false, pointerless: false })).toBe(true);
});

test("a big screen with no pointer is a TV whatever its browser calls itself", () => {
  expect(looksLikeTv({ userAgent: MAC, width: 1920, touch: false, pointerless: true })).toBe(true);
});

test("a desktop browser with a mouse is not a TV, however wide its screen", () => {
  expect(looksLikeTv({ userAgent: MAC, width: 2560, touch: false, pointerless: false })).toBe(false);
});

test("a TV name on a touchscreen or a small screen is not enough", () => {
  expect(looksLikeTv({ userAgent: LG, width: 1920, touch: true, pointerless: false })).toBe(false);
  expect(looksLikeTv({ userAgent: LG, width: 800, touch: false, pointerless: true })).toBe(false);
});

test("a phone is not a TV", () => {
  expect(looksLikeTv({ userAgent: IPHONE, width: 390, touch: true, pointerless: false })).toBe(false);
});
