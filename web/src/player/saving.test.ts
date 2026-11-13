import { expect, test } from "bun:test";
import { type Chance, Unsaved } from "./saving";

type Step = { moved: boolean } | Chance;

const SEEN: Chance = { seen: true, cast: false };
const CASTING: Chance = { seen: true, cast: true };
const HIDDEN: Chance = { seen: false, cast: false };

/** Whether each chance in `steps` saves. A `moved` step is the video reaching a time, playing or not. */
function run(steps: Step[]): boolean[] {
  const unsaved = new Unsaved();
  const saved: boolean[] = [];
  for (const step of steps) {
    if ("moved" in step) unsaved.moved(step.moved);
    else saved.push(unsaved.take(step));
  }
  return saved;
}

test("a playing tab saves at every chance", () => {
  expect(run([{ moved: true }, SEEN, { moved: true }, SEEN])).toEqual([true, true]);
});

test("a paused tab saves where it paused, then nothing during or after a cast of its video", () => {
  expect(run([{ moved: true }, SEEN, CASTING, CASTING, SEEN, SEEN])).toEqual([true, false, false, false, false]);
});

test("a tab that pauses while a TV plays its video saves nothing once the cast ends", () => {
  expect(run([{ moved: true }, CASTING, SEEN])).toEqual([false, false]);
});

test("a tab that plays after the cast ended saves as before", () => {
  expect(run([{ moved: true }, CASTING, SEEN, { moved: true }, SEEN])).toEqual([false, false, true]);
});

test("a seek while paused is not playing, so it saves nothing", () => {
  expect(run([{ moved: false }, SEEN])).toEqual([false]);
});

test("a hidden tab saves nothing, even once it shows again paused", () => {
  expect(run([{ moved: true }, HIDDEN, { moved: true }, HIDDEN, SEEN])).toEqual([false, false, false]);
});
