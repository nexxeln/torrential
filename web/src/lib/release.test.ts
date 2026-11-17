import { describe, expect, test } from "bun:test";
import type { TorrentFile } from "../api";
import { fileUrl, readable, savedVideo } from "./release";

describe("readable", () => {
  test("a scene name reads as its title over its tags", () => {
    expect(readable("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GROUP.mkv")).toEqual({
      title: "Night of the Living Dead",
      tags: "1968 1080p BluRay x264-GROUP",
    });
  });

  test("a year in parentheses keeps neither parenthesis", () => {
    expect(readable("Sintel (2010)")).toEqual({ title: "Sintel", tags: "2010" });
    expect(readable("Night of the Living Dead (1968) [1080p]")).toEqual({ title: "Night of the Living Dead", tags: "1968 [1080p]" });
  });

  test("a title that ends in a period before its tags drops the period", () => {
    expect(readable("Sintel. 2010.1080p")).toEqual({ title: "Sintel", tags: "2010.1080p" });
  });

  test("a name with no tags is all title", () => {
    expect(readable("sintel")).toEqual({ title: "sintel", tags: "" });
  });
});

describe("savedVideo", () => {
  const file = (index: number, path: string, size: number, selected = true): TorrentFile => ({ index, path, size, selected, verified: size });

  test("is the largest video the download kept, not a sample or a skipped file", () => {
    const files = [
      file(0, "Sintel/Sintel.2010.1080p.mkv", 1_200_000_000),
      file(1, "Sintel/Sample/sintel-sample.mkv", 1_500_000_000),
      file(2, "Sintel/Sintel.2010.4k.mkv", 4_000_000_000, false),
      file(3, "Sintel/Sintel.en.srt", 60_000),
    ];
    expect(savedVideo(files)?.index).toBe(0);
  });

  test("is nothing for a download without a video", () => {
    expect(savedVideo([file(0, "ubuntu-24.04.iso", 6_000_000_000)])).toBeNull();
  });

  test("is served at the file route under its own name", () => {
    expect(fileUrl("9b5212", file(4, "Night/Night of the Living Dead (1968).mp4", 1))).toBe("/9b5212/4/Night%20of%20the%20Living%20Dead%20(1968).mp4");
  });
});
