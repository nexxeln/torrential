import { expect, test } from "bun:test";
import { type Box, nearest } from "./remote";

const box = (left: number, top: number, width: number, height: number): Box => ({ left, top, right: left + width, bottom: top + height });

/** A search field over two rows of posters, three and then one, as on the home page. */
const FIELD = box(0, 0, 600, 48);
const POSTERS = [box(0, 100, 180, 270), box(210, 100, 180, 270), box(420, 100, 180, 270), box(0, 400, 180, 270)];

test("down from a wide field lands on the first poster under it, not the one under its middle", () => {
  expect(nearest(FIELD, "down", POSTERS)).toBe(0);
});

test("right and left move along a row of posters and stop at its end", () => {
  expect(nearest(POSTERS[0]!, "right", POSTERS)).toBe(1);
  expect(nearest(POSTERS[1]!, "left", POSTERS)).toBe(0);
  expect(nearest(POSTERS[2]!, "right", POSTERS)).toBe(-1);
});

test("right from a poster stays in its row even when a poster below is closer to its middle", () => {
  expect(nearest(POSTERS[3]!, "right", POSTERS)).toBe(-1);
});

test("down from the end of a row with fewer posters under it goes to the last of them", () => {
  expect(nearest(POSTERS[2]!, "down", POSTERS)).toBe(3);
});

test("up from the first row goes back to the field", () => {
  expect(nearest(POSTERS[1]!, "up", [FIELD, ...POSTERS])).toBe(0);
});

test("a button drawn inside the right end of its row is reached with right, and the row again with left", () => {
  const row = box(0, 0, 600, 56);
  const forget = box(572, 14, 28, 28);
  expect(nearest(row, "right", [forget])).toBe(0);
  expect(nearest(forget, "left", [row])).toBe(0);
});

test("down from a narrow button prefers the full row under it to a control off to the side above that row", () => {
  const sources = box(0, 0, 100, 36);
  const season = box(480, 60, 120, 44);
  const episode = box(0, 120, 600, 90);
  expect(nearest(sources, "down", [season, episode])).toBe(1);
});

test("up from a poster whose name wraps to two lines skips the shorter poster beside it", () => {
  const shorter = box(210, 400, 180, 310);
  const taller = box(420, 400, 180, 330);
  const above = box(420, 100, 260, 200);
  expect(nearest(taller, "up", [shorter, above])).toBe(1);
  expect(nearest(shorter, "down", [taller])).toBe(-1);
});

test("up from a full-width row reaches a small control at the right end of the line above it before the field over that", () => {
  const field = box(0, 0, 1344, 72);
  const video = box(1100, 84, 90, 44);
  const everything = box(1200, 84, 144, 44);
  const row = box(0, 150, 1344, 90);
  expect(nearest(row, "up", [field, video, everything])).toBe(1);
});
