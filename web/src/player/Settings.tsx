import { MinusIcon, PlusIcon } from "@heroicons/react/16/solid";
import type { CSSProperties } from "react";
import type { AudioTrack } from "../api";
import { languageCode, languageName, preferredLanguages } from "../lib/language";
import { preferences, type SubtitleBacking, type SubtitleStyle } from "../lib/storage";
import { SPEEDS } from "./actions";
import { Menu, MenuButton, MenuChoice, MenuLink, MenuNote, MenuRow, MenuSection } from "./Menu";
import type { SubtitleOption, Subtitles } from "./tracks";

/** The pages of the settings popover. `subtitles` also opens straight from its own button. */
export type SettingsPage = "main" | "subtitles" | "languages" | "versions" | "style" | "audio" | "speed";

const SIZES: [short: string, label: string, scale: number][] = [
  ["S", "Small", 0.8],
  ["M", "Medium", 1],
  ["L", "Large", 1.3],
  ["XL", "Extra large", 1.65],
];
const BACKINGS: [label: string, backing: SubtitleBacking][] = [["Shadow", "shadow"], ["Outline", "outline"], ["Box", "box"]];

export function audioLabel(track: AudioTrack): string {
  const language = languageCode(track.language);
  return language ? languageName(language) : (track.title ?? `Track ${track.stream}`);
}

function audioDetail(track: AudioTrack): string {
  const layout = { 1: "mono", 2: "stereo", 6: "5.1", 8: "7.1" }[track.channels] ?? `${track.channels} channels`;
  const title = track.title && languageCode(track.language) ? track.title : null;
  return [title, track.codec.toUpperCase(), layout].filter(Boolean).join(" · ");
}

/** `0.3 s later`, `2.5 s earlier`, or `In sync`. */
export function offsetText(seconds: number): string {
  if (seconds === 0) return "In sync";
  return `${Math.abs(seconds).toFixed(1)} s ${seconds > 0 ? "later" : "earlier"}`;
}

const Online = ({ exact }: { exact: boolean }) => (
  <span className="shrink-0 rounded-md px-1.5 py-0.5 text-xs font-medium whitespace-nowrap text-soft inset-ring inset-ring-white/10 data-exact:text-action data-exact:inset-ring-action/30" data-exact={exact ? "" : undefined}>
    {exact ? "Online · exact" : "Online"}
  </span>
);

interface Props {
  /** The pages opened so far, the current one last. */
  path: SettingsPage[];
  onOpen: (page: SettingsPage) => void;
  onBack: () => void;
  onClose: () => void;
  subtitles: Subtitles;
  /** Seconds the subtitles show later than written. */
  offset: number;
  onShift: (seconds: number) => void;
  onResetOffset: () => void;
  style: SubtitleStyle;
  onStyle: (change: Partial<SubtitleStyle>) => void;
  audioTracks: AudioTrack[];
  audio: number | null;
  onAudio: (stream: number) => void;
  speed: number;
  onSpeed: (speed: number) => void;
}

/** The player's settings: subtitles and their timing and look, the audio track, and the speed. */
export function Settings(props: Props) {
  const { path, onOpen, onClose, subtitles, audioTracks } = props;
  const page = path.at(-1)!;
  const onBack = path.length > 1 ? props.onBack : undefined;
  const { chosen, online } = subtitles;
  const found = online.kind === "found" ? online.options : [];
  /** One row per language: the track in use if it is in that language, else the language's best. */
  const languages = [...new Set(found.map((option) => option.tag!))].sort((a, b) => found.find((o) => o.tag === a)!.label.localeCompare(found.find((o) => o.tag === b)!.label));
  const bestOf = (tag: string) => (chosen?.tag === tag && chosen.source?.kind === "online" ? chosen : found.find((option) => option.tag === tag)!);
  const wanted = [...preferredLanguages(preferences().subtitles), "en"];
  const near = languages.filter((tag) => wanted.includes(languageCode(tag) ?? "") || chosen?.tag === tag);
  const versions = chosen?.source?.kind === "online" ? found.filter((option) => option.tag === chosen.tag) : [];
  const select = (option: SubtitleOption | null) => subtitles.choose(option?.key ?? null);

  const track = (option: SubtitleOption) => (
    <MenuChoice
      key={option.key}
      checked={option.key === chosen?.key}
      disabled={option.source === null}
      detail={option.detail}
      mark={option.source?.kind === "online" ? <Online exact={option.exact} /> : undefined}
      onSelect={() => select(option)}
    >
      {option.label}
    </MenuChoice>
  );

  switch (page) {
    case "main":
      return (
        <Menu page={page} title="Settings" onClose={onClose}>
          <MenuLink label="Subtitles" value={chosen?.label ?? "Off"} onOpen={() => onOpen("subtitles")} />
          {audioTracks.length > 1 && (
            <MenuLink
              label="Audio"
              value={audioLabel(audioTracks.find((t) => t.stream === props.audio) ?? audioTracks[0]!)}
              onOpen={() => onOpen("audio")}
            />
          )}
          <MenuLink label="Speed" value={props.speed === 1 ? "Normal" : `${props.speed}×`} onOpen={() => onOpen("speed")} />
          <MenuLink label="Subtitle style" onOpen={() => onOpen("style")} />
        </Menu>
      );

    case "subtitles":
      return (
        <Menu page={page} title="Subtitles" onBack={onBack} onClose={onClose}>
          <MenuChoice checked={chosen === null} onSelect={() => select(null)}>
            Off
          </MenuChoice>
          {subtitles.local.map(track)}
          <MenuSection title="Found online">
            {online.kind === "looking" && <MenuNote>Looking for subtitles online.</MenuNote>}
            {online.kind === "nothing" && <MenuNote>No subtitles were found online for this video.</MenuNote>}
            {online.kind === "failed" && <MenuNote>The online lookup did not answer.</MenuNote>}
            {near.map((tag) => track(bestOf(tag)))}
            {versions.length > 1 && (
              <MenuLink label={`Other ${chosen!.label} versions`} value={String(versions.length - 1)} onOpen={() => onOpen("versions")} />
            )}
            {languages.length > near.length && (
              <MenuLink label={near.length ? "More languages" : "Languages"} value={String(languages.length - near.length)} onOpen={() => onOpen("languages")} />
            )}
          </MenuSection>
          <MenuSection title="Timing and look">
            {chosen ? (
              <MenuRow label={offsetText(props.offset)}>
                <MenuButton label="Show subtitles 0.1 s earlier (G)" onPress={() => props.onShift(-0.1)}>
                  <MinusIcon className="size-4 fill-ink" />
                </MenuButton>
                <MenuButton label="Show subtitles 0.1 s later (H)" onPress={() => props.onShift(0.1)}>
                  <PlusIcon className="size-4 fill-ink" />
                </MenuButton>
                {props.offset !== 0 && (
                  <MenuButton label="Put subtitles back in sync" onPress={props.onResetOffset}>
                    Reset
                  </MenuButton>
                )}
              </MenuRow>
            ) : (
              <MenuNote>Turn a subtitle on to shift its timing.</MenuNote>
            )}
            <MenuLink label="Style" onOpen={() => onOpen("style")} />
          </MenuSection>
        </Menu>
      );

    case "languages":
      return (
        <Menu page={page} title="Languages" onBack={onBack} onClose={onClose}>
          {languages.filter((tag) => !near.includes(tag)).map((tag) => track(bestOf(tag)))}
        </Menu>
      );

    case "versions":
      return (
        <Menu page={page} title={`${chosen?.label ?? "Online"} versions`} onBack={onBack} onClose={onClose}>
          {versions.map(track)}
          <MenuNote>A version made for another release can run early or late. Shift its timing with G and H.</MenuNote>
        </Menu>
      );

    case "style":
      return (
        <Menu page={page} title="Subtitle style" onBack={onBack} onClose={onClose}>
          <MenuRow label="Size">
            {SIZES.map(([short, label, scale]) => (
              <MenuButton key={short} label={`${label} subtitles`} pressed={props.style.scale === scale} onPress={() => props.onStyle({ scale })}>
                {short}
              </MenuButton>
            ))}
          </MenuRow>
          <MenuRow label="Background">
            {BACKINGS.map(([label, backing]) => (
              <MenuButton key={backing} label={`${label} behind subtitles`} pressed={props.style.backing === backing} onPress={() => props.onStyle({ backing })}>
                {label}
              </MenuButton>
            ))}
          </MenuRow>
          <MenuRow label="Height">
            <input
              type="range"
              name="subtitle-height"
              aria-label="Subtitle height above the bottom edge"
              data-item=""
              min={2}
              max={40}
              step={1}
              value={props.style.lift}
              onChange={(event) => props.onStyle({ lift: Number(event.target.value) })}
              className="slider w-32"
              style={{ "--level": `${((props.style.lift - 2) / 38) * 100}%` } as CSSProperties}
            />
          </MenuRow>
        </Menu>
      );

    case "audio":
      return (
        <Menu page={page} title="Audio" onBack={onBack} onClose={onClose}>
          {audioTracks.map((option) => (
            <MenuChoice key={option.stream} checked={option.stream === props.audio} detail={audioDetail(option)} onSelect={() => props.onAudio(option.stream)}>
              {audioLabel(option)}
            </MenuChoice>
          ))}
        </Menu>
      );

    case "speed":
      return (
        <Menu page={page} title="Speed" onBack={onBack} onClose={onClose}>
          {SPEEDS.map((speed) => (
            <MenuChoice key={speed} checked={speed === props.speed} onSelect={() => props.onSpeed(speed)}>
              {speed === 1 ? "Normal" : `${speed}×`}
            </MenuChoice>
          ))}
        </Menu>
      );
  }
}
