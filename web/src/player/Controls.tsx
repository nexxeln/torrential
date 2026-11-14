import {
  ArrowsPointingInIcon,
  ArrowsPointingOutIcon,
  ArrowTopRightOnSquareIcon,
  ChatBubbleBottomCenterTextIcon,
  Cog6ToothIcon,
  ForwardIcon,
  QueueListIcon,
  SpeakerWaveIcon,
  SpeakerXMarkIcon,
  TvIcon,
} from "@heroicons/react/20/solid";
import { PauseIcon, PlayIcon } from "@heroicons/react/24/solid";
import { type ButtonHTMLAttributes, type CSSProperties, type ReactNode, useState } from "react";
import type { Chapter } from "../api";
import { preferences, savePreferences } from "../lib/storage";
import type { PlayerActions } from "./actions";
import { usePlayer } from "./context";
import { TimeLabel, useChapter } from "./TimeLabel";
import { useVideo } from "./video";

export function ControlButton({ label, className = "", ...rest }: ButtonHTMLAttributes<HTMLButtonElement> & { label: string }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      className={`relative flex size-10 shrink-0 items-center justify-center rounded-full hover:bg-white/15 aria-expanded:bg-white/15 pointer-coarse:size-11 *:[svg]:size-5 *:[svg]:fill-ink ${className}`}
      {...rest}
    />
  );
}

function Volume({ actions }: { actions: PlayerActions }) {
  const { video } = usePlayer();
  const volume = useVideo(video, (state) => (state.muted ? 0 : state.volume));
  return (
    <div className="group/volume flex items-center pointer-coarse:max-sm:hidden tv:hidden">
      <ControlButton label={volume === 0 ? "Unmute (M)" : "Mute (M)"} onClick={actions.toggleMute}>
        {volume === 0 ? <SpeakerXMarkIcon /> : <SpeakerWaveIcon />}
      </ControlButton>
      <div className="flex w-0 items-center overflow-hidden transition-[width] duration-200 group-focus-within/volume:w-24 group-hover/volume:w-24 motion-reduce:transition-none max-sm:hidden">
        <input
          type="range"
          name="volume"
          aria-label="Volume"
          min={0}
          max={1}
          step={0.01}
          value={volume}
          onChange={(event) => actions.setVolume(Number(event.target.value))}
          className="mx-2 slider w-20 shrink-0"
          style={{ "--level": `${volume * 100}%` } as CSSProperties}
        />
      </div>
    </div>
  );
}

function ChapterName({ chapters }: { chapters: Chapter[] }) {
  const chapter = useChapter(chapters);
  if (!chapter?.title) return null;
  return <div className="min-w-0 truncate pl-1 text-sm text-ink/70 max-md:hidden">{chapter.title}</div>;
}

interface Props {
  actions: PlayerActions;
  duration: number | null;
  chapters: Chapter[];
  fullscreen: boolean;
  /** A subtitle track is showing. */
  subtitled: boolean;
  /** The open popover's first page, to mark its button. */
  menu: "subtitles" | "main" | null;
  /** Null where the browser has no way to fill the screen. */
  onFullscreen: (() => void) | null;
  filesOpen: boolean;
  /** More than one video in the torrent. */
  hasFiles: boolean;
  hasNext: boolean;
  nextLabel: string;
  /** Safari can send this video to an AirPlay screen. */
  airPlay: (() => void) | null;
  /** Play on TV. */
  cast: ReactNode;
  /** The timeline, above the buttons. */
  seekBar: ReactNode;
  /** The settings popover, when open. */
  settings: ReactNode;
  onNext: () => void;
  onMenu: (page: "subtitles" | "main") => void;
  onFiles: () => void;
}

/** The bar of controls along the bottom of the picture. */
export function Controls(props: Props) {
  const { actions, menu } = props;
  const paused = useVideo(usePlayer().video, (state) => state.paused || state.ended);
  const [remaining, setRemaining] = useState(() => preferences().showRemaining);
  const clock = (className: string) => (
    <TimeLabel
      duration={props.duration}
      remaining={remaining}
      onFlip={() => {
        savePreferences({ showRemaining: !remaining });
        setRemaining(!remaining);
      }}
      className={className}
    />
  );

  return (
    <div className="relative flex flex-col pr-(--edge-right) pb-(--edge-bottom) pl-(--edge-left)">
      {/* A phone has no room for the clock among the buttons, so it sits beside the timeline. */}
      <div className="flex items-center gap-1">
        <div className="min-w-0 flex-1">{props.seekBar}</div>
        {clock("sm:hidden")}
      </div>
      <div className="flex items-center gap-1 max-sm:gap-0">
        <ControlButton label={paused ? "Play (Space)" : "Pause (Space)"} onClick={() => actions.toggle()} className="*:[svg]:size-6">
          {paused ? <PlayIcon /> : <PauseIcon />}
        </ControlButton>
        {props.hasNext && (
          <ControlButton label={props.nextLabel} onClick={props.onNext}>
            <ForwardIcon />
          </ControlButton>
        )}
        <Volume actions={actions} />
        {clock("max-sm:hidden")}
        <ChapterName chapters={props.chapters} />
        <div className="min-w-0 flex-1" />

        {props.hasFiles && (
          <ControlButton label="Videos in this torrent" aria-expanded={props.filesOpen} onClick={props.onFiles}>
            <QueueListIcon />
          </ControlButton>
        )}
        {/* On a phone the menu hangs from the whole bar, as the buttons are too near the left edge for its width. */}
        <div className="flex items-center gap-1 max-sm:gap-0 sm:relative" data-menu-root>
          <ControlButton
            label="Subtitles (C turns them on or off)"
            aria-haspopup="menu"
            aria-expanded={menu === "subtitles"}
            onClick={() => props.onMenu("subtitles")}
          >
            <ChatBubbleBottomCenterTextIcon />
            {props.subtitled && <span className="absolute inset-x-3 bottom-1 h-0.5 rounded-full bg-action" />}
          </ControlButton>
          <ControlButton label="Settings" aria-haspopup="menu" aria-expanded={menu === "main"} onClick={() => props.onMenu("main")}>
            <Cog6ToothIcon />
          </ControlButton>
          {props.settings}
        </div>
        {props.cast}
        {props.airPlay && (
          <ControlButton label="AirPlay" onClick={props.airPlay}>
            <TvIcon />
          </ControlButton>
        )}
        {document.pictureInPictureEnabled && (
          <ControlButton label="Picture in picture" onClick={actions.togglePictureInPicture} className="max-sm:hidden">
            <ArrowTopRightOnSquareIcon />
          </ControlButton>
        )}
        {props.onFullscreen && (
          <ControlButton label={props.fullscreen ? "Leave fullscreen (F)" : "Fullscreen (F)"} onClick={props.onFullscreen}>
            {props.fullscreen ? <ArrowsPointingInIcon /> : <ArrowsPointingOutIcon />}
          </ControlButton>
        )}
      </div>
    </div>
  );
}
