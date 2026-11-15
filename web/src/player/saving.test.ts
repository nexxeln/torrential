import { expect, test } from "bun:test";
import { type Chance, type Save, Unsaved } from "./saving";

type Step = { moved: boolean } | "resumed" | Chance;

const SEEN: Chance = { seen: true };
const HIDDEN: Chance = { seen: false };
const SAVES: Save = { starts: false };
const STARTS: Save = { starts: true };

/** What each chance in `steps` saves. A `moved` step is the video reaching a time, playing or not. */
function run(steps: Step[]): (Save | null)[] {
  const unsaved = new Unsaved();
  const saved: (Save | null)[] = [];
  for (const step of steps) {
    if (step === "resumed") unsaved.resume();
    else if ("moved" in step) unsaved.moved(step.moved);
    else saved.push(unsaved.take(step));
  }
  return saved;
}

test("a playing tab saves at every chance", () => {
  expect(run([{ moved: true }, SEEN, { moved: true }, SEEN])).toEqual([SAVES, SAVES]);
});

test("a paused tab saves where it paused, then nothing until it plays again", () => {
  expect(run([{ moved: true }, SEEN, SEEN, SEEN])).toEqual([SAVES, null, null]);
});

test("a start or a resume takes the place at once, even on a hidden page", () => {
  expect(run(["resumed", HIDDEN, { moved: true }, SEEN])).toEqual([STARTS, SAVES]);
  expect(run([{ moved: true }, SEEN, "resumed", { moved: true }, SEEN, SEEN])).toEqual([SAVES, STARTS, null]);
});

test("a seek while paused is not playing, so it saves nothing", () => {
  expect(run([{ moved: false }, SEEN])).toEqual([null]);
});

test("a hidden tab saves nothing, even once it shows again paused", () => {
  expect(run([{ moved: true }, HIDDEN, { moved: true }, HIDDEN, SEEN])).toEqual([null, null, null]);
});
