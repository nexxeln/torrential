import { expect, test } from "bun:test";
import { inkOf } from "./Logo";

/** A picture `width` by `height` pixels, clear but for the opaque pixels at `drawn`. */
function picture(width: number, height: number, drawn: [number, number][]): Uint8ClampedArray {
  const pixels = new Uint8ClampedArray(width * height * 4);
  for (const [x, y] of drawn) pixels[(y * width + x) * 4 + 3] = 255;
  return pixels;
}

test("a logo centered in clear margins is measured to its drawn pixels", () => {
  expect(inkOf(picture(8, 4, [[2, 1], [5, 2]]), 8, 4)).toEqual({ left: 0.25, top: 0.25, width: 0.5, height: 0.5, ratio: 2 });
});

test("a faint glow is not counted as drawn", () => {
  const pixels = picture(4, 4, [[1, 1]]);
  pixels[(3 * 4 + 3) * 4 + 3] = 6;
  expect(inkOf(pixels, 4, 4)).toEqual({ left: 0.25, top: 0.25, width: 0.25, height: 0.25, ratio: 1 });
});

test("a picture with nothing drawn is taken whole", () => {
  expect(inkOf(picture(8, 4, []), 8, 4)).toEqual({ left: 0, top: 0, width: 1, height: 1, ratio: 2 });
});
