import { expect, test } from "bun:test";
import { type SubtitleOption, upgrade } from "./tracks";

/** An online subtitle the engine listed for Sintel's sintel-2048-surround.mp4. */
const online = (id: string, tag: string, exact: boolean): SubtitleOption => ({
  key: `online:${id}`,
  label: tag,
  detail: null,
  language: null,
  tag,
  exact,
  source: { kind: "online", id },
});

const byName = online("6738585", "spa", false);
const byHash = [online("3871796", "eng", true), online("3861485", "spa", true), byName];

test("an exact match replaces a track the player picked by itself and leaves one the person picked", () => {
  expect(upgrade(byName, null, byHash)?.key).toBe("online:3861485");
  expect(upgrade(byName, "online:3871796", byHash)?.key).toBe("online:3861485");
  expect(upgrade(byName, "online:6738585", byHash)).toBeNull();
});

test("only an exact match in the same language replaces a track", () => {
  expect(upgrade(byName, null, [online("3871796", "eng", true), byName])).toBeNull();
  expect(upgrade(byHash[1]!, null, byHash)).toBeNull();
  expect(upgrade(null, null, byHash)).toBeNull();
  expect(upgrade(byName, null, byHash)?.tag).toBe("spa");
});
