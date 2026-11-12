import { expect, test } from "bun:test";
import { type Recovery, type RecoveryEvent, recover, WATCHING } from "./recovery";

const NO_SOURCE = { code: "no_source", message: "no other source of The Simpsons S03E05 plays" };

/** The player waiting at 312.5 s with nothing buffered and no bytes arriving, unless `seen` says otherwise. */
function sample(now: number, seen: Partial<Extract<RecoveryEvent, { type: "sample" }>> = {}): RecoveryEvent {
  return { type: "sample", now, time: 312.5, waiting: true, ahead: 0.2, receiving: false, failed: false, ...seen };
}

function run(events: RecoveryEvent[], from: Recovery = WATCHING): Recovery[] {
  const states: Recovery[] = [];
  let state = from;
  for (const event of events) {
    state = recover(state, event);
    states.push(state);
  }
  return states;
}

test("a source that sends nothing for 45 seconds while the video waits is replaced at the playhead", () => {
  expect(run([sample(0, { waiting: false, ahead: 30 }), sample(1000), sample(30_000), sample(45_999), sample(46_000, { time: 313 })])).toEqual([
    { kind: "watching" },
    { kind: "suspect", since: 1000 },
    { kind: "suspect", since: 1000 },
    { kind: "suspect", since: 1000 },
    { kind: "switching", at: 313 },
  ]);
});

test("bytes arriving clear a suspicion", () => {
  expect(run([sample(0), sample(20_000, { receiving: true }), sample(21_000)])).toEqual([
    { kind: "suspect", since: 0 },
    { kind: "watching" },
    { kind: "suspect", since: 21_000 },
  ]);
});

test("a video that plays, is paused, or has a second buffered is not suspected", () => {
  expect(run([sample(0, { waiting: false }), sample(1000, { ahead: 1 }), sample(2000, { waiting: false, ahead: 0 })])).toEqual([
    { kind: "watching" },
    { kind: "watching" },
    { kind: "watching" },
  ]);
});

test("a download that fails is replaced at once, buffered or not", () => {
  expect(run([sample(0, { failed: true, waiting: false, ahead: 20, time: 90 })])).toEqual([{ kind: "switching", at: 90 }]);
  expect(run([sample(0), sample(3000, { failed: true })])).toEqual([
    { kind: "suspect", since: 0 },
    { kind: "switching", at: 312.5 },
  ]);
});

test("the engine's answer ends a switch", () => {
  const switching: Recovery = { kind: "switching", at: 312.5 };
  expect(run([sample(60_000), { type: "playing" }], switching)).toEqual([switching, { kind: "watching" }]);
  expect(run([{ type: "failed", error: NO_SOURCE }], switching)).toEqual([{ kind: "exhausted", error: NO_SOURCE }]);
});

test("with no other source, the player keeps waiting on this one until Retry", () => {
  const exhausted: Recovery = { kind: "exhausted", error: NO_SOURCE };
  expect(run([sample(1000), sample(90_000), { type: "retry", now: 91_000 }, sample(92_000), sample(136_000)], exhausted)).toEqual([
    exhausted,
    exhausted,
    { kind: "suspect", since: 91_000 },
    { kind: "suspect", since: 91_000 },
    { kind: "switching", at: 312.5 },
  ]);
});

test("a source that comes back by itself ends the wait", () => {
  expect(run([sample(1000, { receiving: true })], { kind: "exhausted", error: NO_SOURCE })).toEqual([{ kind: "watching" }]);
});

test("answers and Retry outside their state change nothing", () => {
  expect(run([{ type: "playing" }, { type: "failed", error: NO_SOURCE }, { type: "retry", now: 0 }])).toEqual([
    { kind: "watching" },
    { kind: "watching" },
    { kind: "watching" },
  ]);
});

test("an event that changes nothing returns the same state, so React does not render again", () => {
  const suspect: Recovery = { kind: "suspect", since: 0 };
  expect(recover(suspect, sample(1000))).toBe(suspect);
  expect(recover(WATCHING, sample(0, { waiting: false }))).toBe(WATCHING);
});
