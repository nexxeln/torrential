import { expect, test } from "bun:test";
import { leaveOnBack } from "./keys";

/** A key press as the starting player hears it, which records whether it was cancelled. */
function press(key: string, { keyCode = 0, metaKey = false } = {}) {
  const event = { key, keyCode, metaKey, code: "", ctrlKey: false, altKey: false, shiftKey: false, cancelled: false, preventDefault() {} };
  event.preventDefault = () => void (event.cancelled = true);
  return event;
}

function leave(event: ReturnType<typeof press>): string[] {
  const backs: string[] = [];
  leaveOnBack(() => backs.push("back"))(event as unknown as KeyboardEvent);
  return backs;
}

test("Back on an LG remote leaves the starting player once and keeps the TV browser from going back as well", () => {
  const event = press("Unidentified", { keyCode: 461 });
  expect(leave(event)).toEqual(["back"]);
  expect(event.cancelled).toBe(true);
});

test("Escape leaves the starting player the same way", () => {
  const event = press("Escape");
  expect(leave(event)).toEqual(["back"]);
  expect(event.cancelled).toBe(true);
});

test("Escape with Command held is left to the browser", () => {
  const event = press("Escape", { metaKey: true });
  expect(leave(event)).toEqual([]);
  expect(event.cancelled).toBe(false);
});
