import { useEffect, useState } from "react";
import type { MediaInfo, PlayInfo } from "../api";
import { count, rate } from "../lib/format";
import { usePlayingJob } from "./Chrome";
import { usePlayer } from "./context";
import type { Plan } from "./plan";
import { offsetText } from "./Settings";
import type { SubtitleOption } from "./tracks";
import { bufferedAround } from "./video";

const MODES = {
  direct: "Direct: the browser reads the file",
  copy: "Remux: video as it is, audio to AAC",
  h264: "Transcode: video to H.264, audio to AAC",
};

interface Props {
  media: MediaInfo;
  how: Plan;
  play: PlayInfo;
  subtitle: SubtitleOption | null;
  offset: number;
  /** The audio stream being heard. */
  audio: number | null;
}

/** Playback details for the curious, behind `I`. Reads the element twice a second. */
export function Stats({ media, how, play, subtitle, offset, audio }: Props) {
  const { element } = usePlayer();
  const job = usePlayingJob(play);
  const [, refresh] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => refresh((n) => n + 1), 500);
    return () => clearInterval(timer);
  }, []);

  const { video } = media;
  const heard = media.audio.find((track) => track.stream === audio) ?? media.audio[0];
  const quality = element.getVideoPlaybackQuality();
  const ahead = bufferedAround(element);
  const { progress } = job;
  const rows: [string, string][] = [
    ["Mode", how.kind === "blocked" ? "Cannot play" : MODES[how.kind === "direct" ? "direct" : how.video]],
    ["Video", video ? `${video.codec.toUpperCase()} · ${video.width}×${video.height}${video.fps ? ` · ${Number(video.fps.toFixed(3))} fps` : ""}${video.hdr ? " · HDR" : ""}` : "Unknown"],
    ["Audio", heard ? `${heard.codec.toUpperCase()} · ${heard.channels} channels` : "None"],
    ["Container", media.container],
    ["Buffer", ahead ? `${(ahead[1] - element.currentTime).toFixed(1)} s ahead` : "Empty"],
    ["Frames", `${quality.droppedVideoFrames} dropped of ${quality.totalVideoFrames}`],
    ["Download", job.state === "downloading" ? `${rate(progress.down_rate)} from ${count(progress.peers, "peer")}` : job.state],
    ["Subtitles", subtitle ? `${subtitle.label} · ${subtitle.source?.kind ?? "none"} · ${offsetText(offset).toLowerCase()}` : "Off"],
  ];

  return (
    <dl
      aria-label="Playback details"
      className="absolute top-20 left-(--edge-left) z-10 grid max-w-[calc(100%-(--spacing(6)))] animate-pop grid-cols-[auto_1fr] gap-x-4 gap-y-1 rounded-xl bg-black/70 px-4 py-3 text-xs tabular-nums backdrop-blur-md motion-reduce:animate-none"
    >
      {rows.map(([name, value]) => (
        <div key={name} className="col-span-2 grid grid-cols-subgrid">
          <dt className="text-ink/60">{name}</dt>
          <dd className="truncate text-ink">{value}</dd>
        </div>
      ))}
    </dl>
  );
}
