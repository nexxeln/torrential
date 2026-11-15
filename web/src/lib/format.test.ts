import { expect, test } from "bun:test";
import vectors from "../../../crates/core/tests/fixtures/format.json";
import { age, bytes, clock, percent } from "./format";
import { isFinished } from "./storage";

test("sizes, ages, percents and clocks read as the shared vectors say", () => {
  for (const { bytes: count, web } of vectors.size) expect([count, bytes(count)]).toEqual([count, web]);
  for (const { seconds, web } of vectors.age) expect([seconds, age(0, seconds)]).toEqual([seconds, web]);
  for (const { done, size, web } of vectors.percent) expect([done, size, percent(done / size)]).toEqual([done, size, web]);
  for (const { seconds, text } of vectors.clock) expect([seconds, clock(seconds)]).toEqual([seconds, text]);
});

test("a video is finished where the shared vectors say", () => {
  for (const { position, duration, finished } of vectors.finished) {
    expect([position, duration, isFinished(position, duration)]).toEqual([position, duration, finished]);
  }
});
