import { MinusIcon, PlusIcon } from "@heroicons/react/16/solid";
import { PlayIcon } from "@heroicons/react/24/solid";
import {
  memo,
  type PointerEvent,
  type ReactNode,
  type RefObject,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import type { MediaInfo, PlayInfo } from "../api";
import { clock } from "../lib/format";
import { fileName, isVideoFile, readable } from "../lib/release";
import {
  forgetPosition,
  position,
  preferences,
  savePosition,
  savePreferences,
  saveSubtitleOffset,
  subtitleOffset,
  type SubtitleStyle,
} from "../lib/storage";
import { navigate, watchPath } from "../router";
import { Button } from "../ui/Button";
import { PlayerActions } from "./actions";
import { Buffering, Ended, PlayingReadout, Problem, Starting, TopBar } from "./Chrome";
import { ControlButton, Controls } from "./Controls";
import { type PlayerParts, PlayerContext, Scrub } from "./context";
import { CueTrack } from "./cues";
import { FeedbackLayer } from "./FeedbackLayer";
import { FeedbackStore } from "./feedback";
import { FilesPanel } from "./FilesPanel";
import { type Command, command } from "./keys";
import { defaultAudio, plan } from "./plan";
import { SeekBar } from "./SeekBar";
import { audioLabel, offsetText, Settings, type SettingsPage } from "./Settings";
import { Shortcuts } from "./Shortcuts";
import { DirectSource, RemuxSource, type Source, type SourceState } from "./source";
import { Stats } from "./Stats";
import { SubtitleLayer } from "./SubtitleLayer";
import { useAirPlay, useMediaSession, useWakeLock } from "./system";
import { useSubtitleTracks } from "./tracks";
import { useVideo, VideoStore } from "./video";

/** The controls hide after this long without input while the video plays. */
const IDLE_MS = 2500;
/** A stored position this early is not worth resuming. */
const WORTH_RESUMING = 10;
const SAVE_EVERY_MS = 5000;
/** A click waits this long for a second one before it plays or pauses, so a double click only toggles fullscreen. */
const DOUBLE_MS = 220;
/** Scroll-wheel distance for one 5% step of volume. */
const WHEEL_STEP = 60;
const HINT_MS = 12000;

/** A name for what plays, over the picture. */
export interface Heading {
  title: string;
  /** Under the title: an episode's number and name, or a film's year. */
  detail: string | null;
}

/** Where a video starts, and where the place it stopped is kept. */
export interface Memory {
  /** Seconds. */
  start: number;
  /** Starting at `start` picks up an earlier visit, which the player says, with a way to start over. */
  resumed: boolean;
  save: (time: number, duration: number) => void;
  forget: () => void;
}

/** A video before or after this one. */
export interface Neighbor {
  name: string;
  go: () => void;
}

/** A film or an episode played by title, instead of a file of a torrent. */
export interface Titled {
  kind: "film" | "episode";
  heading: Heading;
  /** The episodes N and P move to, and the one the end of the video counts down to. */
  previous: Neighbor | null;
  next: Neighbor | null;
  memory: Memory;
  /** Shown before the first frame. */
  cover: ReactNode;
  /** A picture to show until the source's own first frame, such as the last frame of the source it replaces. */
  poster: string | null;
  /** More layers over the picture, which can use the player's context. */
  overlay: ReactNode;
  /** Another source is being found, which the overlay says, so the wait says nothing about this one's swarm. */
  replacing: boolean;
}

interface Props {
  play: PlayInfo;
  media: MediaInfo;
  /** Asks the engine for the video again, from the start of the session. */
  onReload: () => void;
  onBack: () => void;
  /** Without it, the player names the release, moves through the torrent's videos, and keeps positions in this browser. */
  titled?: Titled;
}

function useFullscreen(target: RefObject<HTMLElement | null>): [boolean, () => void] {
  const [active, setActive] = useState(false);
  useEffect(() => {
    const changed = () => setActive(document.fullscreenElement === target.current);
    document.addEventListener("fullscreenchange", changed);
    return () => document.removeEventListener("fullscreenchange", changed);
  }, [target]);
  const toggle = useCallback(() => {
    if (document.fullscreenElement) void document.exitFullscreen();
    else void target.current?.requestFullscreen().catch(() => {});
  }, [target]);
  return [active, toggle];
}

/** True for `IDLE_MS` after each `wake`. `sleep` ends it at once. */
function useAwake(): [boolean, () => void, () => void] {
  const [awake, setAwake] = useState(true);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const wake = useCallback(() => {
    setAwake(true);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setAwake(false), IDLE_MS);
  }, []);
  const sleep = useCallback(() => {
    clearTimeout(timer.current);
    setAwake(false);
  }, []);
  useEffect(() => {
    wake();
    return () => clearTimeout(timer.current);
  }, [wake]);
  return [awake, wake, sleep];
}

/** A file of a season by its own name: the episode, without the show's title that its release already says. */
function episodeName(path: string, release: string): string {
  const own = readable(fileName(path));
  const whole = `${own.title} ${own.tags}`.trim();
  const show = readable(release).title;
  return (whole.startsWith(show) && whole.slice(show.length).trim()) || whole;
}

/** A torrent's file starts where this browser saw it stop, and keeps its place there. */
function torrentMemory(play: PlayInfo): Memory {
  const { id, name, video: file } = play;
  const stored = position(id, file.index);
  const start = stored && !stored.finished && stored.time > WORTH_RESUMING ? stored.time : 0;
  return {
    start,
    resumed: start > 0,
    save: (time, duration) => savePosition({ id, file: file.index, name, path: file.path, time, duration }),
    forget: () => forgetPosition(id, file.index),
  };
}

/**
 * The player once the engine has said what is in the file: the picture, and everything drawn over
 * it. Memoized, because the page above renders with every report of the download's progress.
 */
export const Playback = memo(function Playback(props: Props) {
  const frame = useRef<HTMLDivElement>(null);
  const [element, setElement] = useState<HTMLVideoElement | null>(null);
  return (
    <div ref={frame} className="relative size-full overflow-hidden bg-black select-none">
      <video ref={setElement} playsInline poster={props.titled?.poster ?? undefined} className="absolute inset-0 size-full" />
      {element && <Stage element={element} frame={frame} {...props} />}
    </div>
  );
});

function Stage({ element, frame, play, media, onReload, onBack, titled }: Props & { element: HTMLVideoElement; frame: RefObject<HTMLDivElement | null> }) {
  const { id, video: file } = play;
  const parts = useMemo<PlayerParts>(
    () => ({ element, video: new VideoStore(element), feedback: new FeedbackStore(), scrub: new Scrub() }),
    [element],
  );
  const { video, feedback, scrub } = parts;
  const paused = useVideo(video, (state) => state.paused);
  const started = useVideo(video, (state) => state.started);
  const ended = useVideo(video, (state) => state.ended);
  const waiting = useVideo(video, (state) => state.waiting);
  const speed = useVideo(video, (state) => state.speed);
  const ownDuration = useVideo(video, (state) => state.duration);
  const duration = media.duration ?? ownDuration;
  const scrubbing = useSyncExternalStore(scrub.subscribe, scrub.active);
  const [fullscreen, toggleFullscreen] = useFullscreen(frame);
  const [awake, wake, sleep] = useAwake();

  const ownMemory = titled?.memory;
  const memory = useMemo(() => ownMemory ?? torrentMemory(play), [ownMemory, play]);
  /** The last time played, which a new source starts from and the position memory stores. */
  const lastTime = useRef(memory.start);
  const [resumeNotice, setResumeNotice] = useState(memory.resumed);

  const timeline = useRef({ duration, fps: media.video?.fps ?? null, chapters: media.chapters });
  timeline.current = { duration, fps: media.video?.fps ?? null, chapters: media.chapters };
  const actions = useMemo(() => new PlayerActions(element, feedback, () => timeline.current), [element, feedback]);

  // Which way the file reaches the element.
  const [audio, setAudio] = useState<number | null>(null);
  const [directWorks, setDirectWorks] = useState(true);
  const how = useMemo(() => plan(media, audio, directWorks), [media, audio, directWorks]);
  const heard = audio ?? defaultAudio(media.audio)?.stream ?? null;
  const [sourceState, setSourceState] = useState<SourceState>({ kind: "streaming" });
  const source = useRef<Source | null>(null);
  /** The browser refused to start without a click. */
  const [needsClick, setNeedsClick] = useState(false);
  const howKind = how.kind;
  const type = how.kind === "remux" ? how.type : null;
  const videoMode = how.kind === "remux" ? how.video : null;
  useEffect(() => {
    if (howKind === "blocked") return;
    const start = lastTime.current;
    const made =
      howKind === "direct"
        ? new DirectSource(element, file.url, start, setSourceState)
        : new RemuxSource(
            element,
            { id, file: file.index, duration: media.duration, type: type!, video: videoMode!, audio: heard ?? undefined, start },
            setSourceState,
          );
    source.current = made;
    const remembered = preferences();
    element.volume = remembered.volume;
    element.muted = remembered.muted;
    element.defaultPlaybackRate = element.playbackRate = remembered.speed;
    element.play().then(
      () => setNeedsClick(false),
      (refused: DOMException) => setNeedsClick(refused.name === "NotAllowedError"),
    );
    return () => {
      source.current = null;
      made.destroy();
    };
  }, [element, howKind, type, videoMode, heard, id, file.index, file.url, media.duration]);

  // A file the browser said it could read, but could not: let the engine repackage it.
  const failure = sourceState.kind === "failed" ? sourceState.error : null;
  useEffect(() => {
    if (failure && howKind === "direct" && media.remux && /^media_error_[34]$/.test(failure.code)) setDirectWorks(false);
  }, [failure, howKind, media.remux]);

  // Where playback is, remembered for the next visit.
  const remember = useCallback(() => {
    if (!duration || lastTime.current <= WORTH_RESUMING / 2) return;
    memory.save(lastTime.current, duration);
  }, [memory, duration]);
  useEffect(() => {
    const follow = () => {
      if (video.snapshot().started) lastTime.current = element.ended && duration ? duration : element.currentTime;
    };
    const stop = new AbortController();
    for (const name of ["timeupdate", "ended"]) element.addEventListener(name, follow, { signal: stop.signal });
    for (const name of ["pause", "ended"]) element.addEventListener(name, remember, { signal: stop.signal });
    addEventListener("pagehide", remember, { signal: stop.signal });
    const timer = setInterval(remember, SAVE_EVERY_MS);
    return () => {
      stop.abort();
      clearInterval(timer);
      remember();
    };
  }, [element, video, duration, remember]);
  useEffect(() => {
    if (!resumeNotice) return;
    const timer = setTimeout(() => setResumeNotice(false), 8000);
    return () => clearTimeout(timer);
  }, [resumeNotice]);

  // Subtitles: which track, its cues, its timing, and its look.
  const subtitles = useSubtitleTracks(play, media);
  const { chosen } = subtitles;
  const [track, setTrack] = useState<CueTrack | null>(null);
  const chosenKey = chosen?.key ?? null;
  const chosenSource = chosen?.source ?? null;
  useEffect(() => {
    if (!chosenSource) return setTrack(null);
    const made = new CueTrack(element, id, file.index, chosenSource);
    setTrack(made);
    return () => made.destroy();
    // The key stands for the source, which is a new object on every render.
  }, [element, id, file.index, chosenKey]);

  const storedOffset = useMemo(() => (chosenKey ? subtitleOffset(id, file.index, chosenKey) : 0), [id, file.index, chosenKey]);
  const [shifted, setShifted] = useState<{ key: string; seconds: number } | null>(null);
  const offset = shifted && shifted.key === chosenKey ? shifted.seconds : storedOffset;
  const setOffset = useCallback(
    (seconds: number) => {
      if (!chosenKey) return feedback.note("Subtitles are off");
      const next = Math.round(seconds * 10) / 10;
      setShifted({ key: chosenKey, seconds: next });
      saveSubtitleOffset(id, file.index, chosenKey, next);
      feedback.note(next === 0 ? "Subtitles in sync" : `Subtitles ${offsetText(next)}`);
    },
    [chosenKey, id, file.index, feedback],
  );
  const [style, setStyle] = useState<SubtitleStyle>(() => preferences().subtitleStyle);
  const restyle = useCallback((change: Partial<SubtitleStyle>) => {
    setStyle((before) => {
      const next = { ...before, ...change };
      savePreferences({ subtitleStyle: next });
      return next;
    });
  }, []);

  // An online track made for another release can be early or late. Say so once, with the fix at hand.
  const [hint, setHint] = useState<string | null>(null);
  const loose = chosen?.source?.kind === "online" && !chosen.exact ? chosen.key : null;
  /** The track last announced. Undefined before the first render, which announces nothing. */
  const announced = useRef<string | null | undefined>(undefined);
  useEffect(() => {
    if (chosenKey === announced.current) return;
    const first = announced.current === undefined;
    announced.current = chosenKey;
    if (!first) feedback.note(chosen ? `Subtitles: ${chosen.label}${chosen.source?.kind === "online" ? ", found online" : ""}` : "Subtitles off");
    // A track already shifted on an earlier visit needs no telling.
    setHint(loose && subtitleOffset(id, file.index, loose) === 0 ? loose : null);
  }, [chosenKey, chosen, loose, feedback, id, file.index]);
  useEffect(() => {
    if (!hint) return;
    const timer = setTimeout(() => setHint(null), HINT_MS);
    return () => clearTimeout(timer);
  }, [hint, offset]);

  // What is open over the picture.
  const [menu, setMenu] = useState<SettingsPage[]>([]);
  const [filesOpen, setFilesOpen] = useState(false);
  const [sheet, setSheet] = useState(false);
  const [stats, setStats] = useState(false);
  /** The pointer rests on the controls, which keeps them shown. */
  const [onControls, setOnControls] = useState(false);
  const covered = useRef(false);
  covered.current = menu.length > 0 || filesOpen;
  const closeMenu = useCallback(() => {
    const root = frame.current?.querySelector("[data-menu-root]");
    const within = root?.contains(document.activeElement);
    const opener = root?.querySelector<HTMLElement>("[aria-expanded=true]");
    setMenu([]);
    if (within) opener?.focus();
  }, [frame]);
  const openMenu = useCallback((page: "subtitles" | "main") => setMenu((path) => (path[0] === page ? [] : [page])), []);

  const videos = useMemo(
    () => play.files.filter((f) => isVideoFile(f.path)).sort((a, b) => a.path.localeCompare(b.path, "en", { numeric: true })),
    [play.files],
  );
  const here = videos.findIndex((f) => f.index === file.index);
  const nextFile = videos[here + 1] ?? null;
  const previousFile = videos[here - 1] ?? null;
  const torrentNext = useMemo<Neighbor | null>(
    () => nextFile && { name: episodeName(nextFile.path, play.name), go: () => navigate(watchPath(id, nextFile.index), { replace: true }) },
    [id, nextFile, play.name],
  );
  const torrentPrevious = useMemo<Neighbor | null>(
    () => previousFile && { name: episodeName(previousFile.path, play.name), go: () => navigate(watchPath(id, previousFile.index), { replace: true }) },
    [id, previousFile, play.name],
  );
  const next = titled ? titled.next : torrentNext;
  const previous = titled ? titled.previous : torrentPrevious;
  const goNext = next?.go ?? null;
  const goPrevious = previous?.go ?? null;
  const multiple = !titled && videos.length > 1;
  const ends =
    titled?.kind === "episode"
      ? ["This is the last episode", "This is the first episode"]
      : titled
        ? ["Nothing plays after this film", "Nothing plays before this film"]
        : ["This is the last video of the torrent", "This is the first video of the torrent"];

  const run = useCallback(
    (action: Command) => {
      switch (action.kind) {
        case "toggle":
          return actions.toggle(true);
        case "seekBy":
          return actions.seekBy(action.seconds);
        case "seekTo":
          return actions.seekToFraction(action.fraction);
        case "step":
          return actions.step(action.direction);
        case "chapter":
          return actions.chapter(action.direction);
        case "volumeBy":
          return actions.volumeBy(action.amount);
        case "mute":
          return actions.toggleMute();
        case "speedBy":
          return actions.speedBy(action.steps);
        case "fullscreen":
          return toggleFullscreen();
        case "file": {
          const go = action.direction === 1 ? goNext : goPrevious;
          return go ? go() : feedback.note(action.direction === 1 ? ends[0]! : ends[1]!);
        }
        case "subtitles":
          if (subtitles.toggle() === "none") {
            feedback.note(subtitles.online.kind === "looking" ? "Still looking for subtitles online" : "No subtitles for this video");
          }
          return;
        case "subtitleDelay":
          return setOffset(offset + action.seconds);
        case "audio": {
          if (media.audio.length < 2) return feedback.note("This video has one audio track");
          const now = media.audio.findIndex((option) => option.stream === heard);
          const pick = media.audio[(now + 1) % media.audio.length]!;
          setAudio(pick.stream);
          return feedback.note(`Audio: ${audioLabel(pick)}`);
        }
        case "stats":
          return setStats((open) => !open);
        case "shortcuts":
          return setSheet((open) => !open);
        case "leave":
          if (sheet) return setSheet(false);
          if (menu.length) return closeMenu();
          if (filesOpen) return setFilesOpen(false);
          return onBack();
      }
    },
    [actions, feedback, subtitles, offset, setOffset, media.audio, heard, goNext, goPrevious, ends, toggleFullscreen, sheet, menu.length, filesOpen, closeMenu, onBack],
  );
  useEffect(() => {
    const pressed = (event: KeyboardEvent) => {
      wake();
      const action = command(event);
      // The shortcut sheet is a dialog: under it, only the keys that close it act.
      if (!action || (sheet && action.kind !== "leave" && action.kind !== "shortcuts")) return;
      event.preventDefault();
      run(action);
    };
    addEventListener("keydown", pressed);
    return () => removeEventListener("keydown", pressed);
  }, [run, wake, sheet]);

  const nowPlaying = titled
    ? titled.kind === "episode"
      ? { title: titled.heading.detail ?? titled.heading.title, album: titled.heading.title }
      : { title: titled.heading.title, album: "" }
    : { title: multiple ? episodeName(file.path, play.name) : readable(play.name).title, album: readable(play.name).title };
  useMediaSession({ ...nowPlaying, actions, video, onNext: goNext, onPrevious: goPrevious });
  useWakeLock(video);
  const airPlay = useAirPlay(element, how.kind === "direct");

  // The pointer on the picture: a click plays or pauses, a double click is fullscreen, the wheel is volume.
  const click = useRef<ReturnType<typeof setTimeout>>(undefined);
  const taps = useRef({ at: 0, x: 0 });
  const wheel = useRef(0);
  const pressed = (event: PointerEvent<HTMLDivElement>) => {
    if (event.pointerType === "mouse") return;
    // Touch: a tap shows or hides the controls, and a second tap soon after seeks on the side it lands.
    const box = event.currentTarget.getBoundingClientRect();
    const side = (event.clientX - box.left) / box.width;
    const again = performance.now() - taps.current.at < 320 && Math.abs(side - taps.current.x) < 0.2;
    taps.current = { at: again ? 0 : performance.now(), x: side };
    if (!again) return awake ? sleep() : wake();
    wake();
    if (side < 0.35) actions.seekBy(-10);
    else if (side > 0.65) actions.seekBy(10);
    else actions.toggle(true);
  };
  const clicked = (count: number, mouse: boolean) => {
    if (covered.current) return count === 1 ? (closeMenu(), setFilesOpen(false)) : undefined;
    if (!mouse) return;
    const waitingClick = click.current !== undefined;
    clearTimeout(click.current);
    click.current = undefined;
    if (count === 1) {
      click.current = setTimeout(() => {
        click.current = undefined;
        actions.toggle(true);
      }, DOUBLE_MS);
    } else if (count === 2) {
      // The first click already played or paused if the second came late. Put that back.
      if (!waitingClick) actions.toggle();
      toggleFullscreen();
    }
  };
  useEffect(() => () => clearTimeout(click.current), []);
  const lastPointer = useRef("mouse");

  const blocked = how.kind === "blocked" ? how.reason : null;
  const stuck = failure !== null || blocked !== null;
  const starting = !stuck && !started && waiting;
  const showControls = !stuck && !starting && (awake || onControls || paused || menu.length > 0 || scrubbing);
  const hidden = showControls || stuck || starting ? undefined : "";

  return (
    <PlayerContext value={parts}>
      <div
        data-idle={showControls || stuck || starting ? undefined : ""}
        onPointerMove={(event) => event.pointerType === "mouse" && wake()}
        className="absolute inset-0 data-idle:cursor-none"
      >
        <div
          className="absolute inset-0"
          onPointerUp={(event) => {
            lastPointer.current = event.pointerType;
            pressed(event);
          }}
          onClick={(event) => clicked(event.detail, lastPointer.current === "mouse")}
          onWheel={(event) => {
            wheel.current += event.deltaY;
            const steps = Math.trunc(wheel.current / WHEEL_STEP);
            if (steps === 0) return;
            wheel.current -= steps * WHEEL_STEP;
            actions.volumeBy(-steps * 0.05);
          }}
        />

        <SubtitleLayer track={track} offset={offset} style={style} lifted={showControls} sample={menu.at(-1) === "style"} />
        {starting && (titled ? titled.cover : <Starting step={2} name={play.name} readout={<PlayingReadout play={play} />} />)}
        {!stuck && <Buffering play={play} quiet={titled?.replacing ?? false} />}
        <FeedbackLayer />

        {!stuck && !waiting && paused && needsClick && (
          <button
            type="button"
            aria-label="Play"
            onClick={() => actions.toggle()}
            className="absolute top-1/2 left-1/2 flex size-20 -translate-1/2 items-center justify-center rounded-full bg-black/55 inset-ring inset-ring-white/20 backdrop-blur-sm hover:bg-black/70"
          >
            <PlayIcon className="size-6 fill-ink" />
          </button>
        )}
        {!stuck && ended && (
          <Ended
            next={next?.name ?? null}
            onNext={goNext ?? onBack}
            onReplay={() => (actions.seekTo(0), actions.play())}
            onBack={onBack}
            back={titled ? "Back" : undefined}
          />
        )}

        <div
          data-hidden={hidden}
          className="absolute inset-x-0 top-0 z-10 transition-opacity duration-300 data-hidden:pointer-events-none data-hidden:opacity-0 motion-reduce:transition-none"
        >
          <TopBar
            name={starting || titled ? null : play.name}
            heading={starting ? null : titled?.heading}
            file={multiple ? fileName(file.path) : null}
            readout={starting ? null : <PlayingReadout play={play} />}
            onBack={onBack}
          />
        </div>
        {stats && <Stats media={media} how={how} play={play} subtitle={chosen} offset={offset} audio={heard} />}

        {titled?.overlay}
        {failure && <Problem title="The video stopped" error={failure} onRetry={() => source.current?.retry()} onBack={onBack} back={titled ? "Back" : undefined} />}
        {blocked === "ffmpeg" && (
          <Problem
            title="This file needs ffmpeg"
            fix={/mac/i.test(navigator.userAgent) ? "brew install ffmpeg" : "sudo apt install ffmpeg"}
            note={
              <>
                To watch in a desktop player instead, run <code className="font-mono">torrential play {id.slice(0, 6)}</code>.
              </>
            }
            onRetry={onReload}
            onBack={onBack}
            back={titled ? "Back" : undefined}
          >
            <p className="text-sm text-pretty text-soft">
              Browsers cannot read {media.container === "matroska" ? "MKV" : media.container} files by themselves. torrential
              repackages them with ffmpeg as they download. Install it, then try again.
            </p>
          </Problem>
        )}
        {blocked === "browser" && (
          <Problem title="This browser cannot play the video" onRetry={onReload} onBack={onBack} back={titled ? "Back" : undefined}>
            <p className="text-sm text-pretty text-soft">
              It has no decoder for {media.video?.codec.toUpperCase() ?? "this format"}. Try another browser, or run{" "}
              <code className="font-mono">torrential play {id.slice(0, 6)}</code> to watch in a desktop player.
            </p>
          </Problem>
        )}

        {hint && hint === chosenKey && !stuck && menu.length === 0 && (
          <div
            role="group"
            aria-label="Subtitle timing"
            className="absolute right-[max(--spacing(3),2vw)] bottom-28 z-10 flex animate-appear items-center gap-3 rounded-xl bg-black/70 py-2 pr-2 pl-4 text-sm text-ink backdrop-blur-md motion-reduce:animate-none"
          >
            <div className="flex flex-col">
              <p className="font-medium tabular-nums">{offset === 0 ? "Out of sync?" : `Subtitles ${offsetText(offset)}`}</p>
              <p className="text-xs text-ink/65">Shift them with G and H, or here.</p>
            </div>
            <div className="flex items-center gap-1">
              <ControlButton label="Show subtitles 0.1 s earlier (G)" onClick={() => setOffset(offset - 0.1)} className="size-8 *:[svg]:size-4">
                <MinusIcon />
              </ControlButton>
              <ControlButton label="Show subtitles 0.1 s later (H)" onClick={() => setOffset(offset + 0.1)} className="size-8 *:[svg]:size-4">
                <PlusIcon />
              </ControlButton>
            </div>
          </div>
        )}

        <div
          data-hidden={showControls ? undefined : ""}
          inert={!showControls}
          onPointerEnter={(event) => setOnControls(event.pointerType === "mouse")}
          onPointerLeave={() => setOnControls(false)}
          className="absolute inset-x-0 bottom-0 z-10 bg-linear-to-t from-black/85 via-black/45 to-transparent pt-20 transition-opacity duration-300 data-hidden:pointer-events-none data-hidden:opacity-0 motion-reduce:transition-none"
        >
          {resumeNotice && (
            <div className="flex px-[max(--spacing(3),2vw)] pb-1">
              <div className="flex animate-appear items-center gap-1 rounded-lg bg-black/60 py-1 pr-1 pl-3 text-sm text-ink tabular-nums motion-reduce:animate-none">
                Resumed from {clock(memory.start)}
                <Button
                  variant="quiet"
                  className="px-2 py-0.5"
                  onClick={() => {
                    memory.forget();
                    setResumeNotice(false);
                    actions.seekTo(0);
                  }}
                >
                  Start over
                </Button>
              </div>
            </div>
          )}
          <Controls
            actions={actions}
            duration={duration}
            chapters={media.chapters}
            fullscreen={fullscreen}
            subtitled={chosen !== null}
            menu={menu[0] === "subtitles" || menu[0] === "main" ? menu[0] : null}
            filesOpen={filesOpen}
            hasFiles={multiple}
            hasNext={next !== null}
            nextLabel={titled ? "Next episode (N)" : "Next video (N)"}
            airPlay={airPlay}
            onNext={goNext ?? onBack}
            onMenu={openMenu}
            onFiles={() => setFilesOpen((open) => !open)}
            onFullscreen={toggleFullscreen}
            seekBar={
              <SeekBar
                play={play}
                duration={duration}
                chapters={media.chapters}
                frames={media.remux}
                aspect={media.video && media.video.height > 0 ? media.video.width / media.video.height : 16 / 9}
                onSeek={actions.seekTo}
              />
            }
            settings={
              menu.length > 0 && (
                <Settings
                  path={menu}
                  onOpen={(page) => setMenu((path) => [...path, page])}
                  onBack={() => setMenu((path) => path.slice(0, -1))}
                  onClose={closeMenu}
                  subtitles={subtitles}
                  offset={offset}
                  onShift={(seconds) => setOffset(offset + seconds)}
                  onResetOffset={() => setOffset(0)}
                  style={style}
                  onStyle={restyle}
                  audioTracks={media.audio}
                  audio={heard}
                  onAudio={(stream) => {
                    setAudio(stream);
                    feedback.note(`Audio: ${audioLabel(media.audio.find((option) => option.stream === stream)!)}`);
                  }}
                  speed={speed}
                  onSpeed={actions.setSpeed}
                />
              )
            }
          />
        </div>

        {filesOpen && <FilesPanel id={id} videos={videos} current={file.index} onClose={() => setFilesOpen(false)} />}
        {sheet && <Shortcuts onClose={() => setSheet(false)} />}
      </div>
    </PlayerContext>
  );
}
