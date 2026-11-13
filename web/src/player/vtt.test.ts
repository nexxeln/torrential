import { expect, test } from "bun:test";
import { cueText, VttParser } from "./vtt";

function only(text: string) {
  const parser = new VttParser();
  return [...parser.feed(text), ...parser.finish()];
}

test("a cue goes back to WebVTT text with its styles and its escaped characters", () => {
  const [cue] = only("WEBVTT\n\n00:00:01.000 --> 00:00:03.000\n<i>Tom &amp; Jerry</i> are <b>back</b>\nfor 2 &lt; 3 days\n");
  expect(cueText(cue!)).toBe("<i>Tom &amp; Jerry</i> are <b>back</b>\nfor 2 &lt; 3 days");
});

test("a run in two styles keeps both", () => {
  const [cue] = only("WEBVTT\n\n00:00:01.000 --> 00:00:03.000\n<i><u>Quiet</u></i>\n");
  expect(cueText(cue!)).toBe("<i><u>Quiet</u></i>");
});
