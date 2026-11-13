import { describe, expect, test } from "bun:test";
import { trashed } from "./trashed";

const change = { id: "ab12", name: "Sintel", result: "removed" };

describe("trashed", () => {
  test("files that all went to the Trash say so", () => {
    expect(trashed("Sintel", { ...change, trashed: ["/dl/Sintel/sintel.mkv", "/dl/Sintel/sintel.srt"] })).toBe(
      "The files of Sintel are in the Trash.",
    );
  });

  test("files that stayed say how many and why", () => {
    const kept = [{ path: "/dl/Sintel/sintel.srt", reason: "another download uses it" }];
    expect(trashed("Sintel", { ...change, trashed: ["/dl/Sintel/sintel.mkv"], kept })).toBe(
      "The files of Sintel are in the Trash, except 1 file that stayed, for another download uses it.",
    );
    expect(trashed("Sintel", { ...change, kept: [{ path: "/dl/Sintel/sintel.mkv", reason: "there is no Trash on the disk it is on" }] })).toBe(
      "The files of Sintel stayed, for there is no Trash on the disk it is on.",
    );
  });

  test("a download that wrote nothing says it had no files", () => {
    expect(trashed("Sintel", change)).toBe("Sintel had no files on disk.");
  });
});
