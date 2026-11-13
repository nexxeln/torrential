import { describe, expect, test } from "bun:test";
import { readable } from "./release";

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
