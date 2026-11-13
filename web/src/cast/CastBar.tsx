import { BackwardIcon, ForwardIcon, SpeakerWaveIcon, StopIcon, TvIcon, XMarkIcon } from "@heroicons/react/20/solid";
import { PauseIcon, PlayIcon } from "@heroicons/react/24/solid";
import { type CSSProperties, useState } from "react";
import { asApiError, type Cast, type CastAction, controlCast } from "../api";
import { clock, sentence } from "../lib/format";
import { ControlButton } from "../player/Controls";
import { useRoute } from "../router";
import { Spinner } from "../ui/Spinner";
import { changed, useTvList } from "./casts";

function key(cast: Cast): string {
  return `${cast.tv}:${cast.started_at}`;
}

/**
 * Along the bottom of every page but the player's, a bar for each cast playing on a TV, with its
 * remote control. A cast that failed stays until it is dismissed.
 */
export function CastBar() {
  const { casts, tvs } = useTvList();
  const route = useRoute();
  const [dismissed, setDismissed] = useState<string[]>([]);
  const shown = casts.filter((cast) => (cast.state !== "ended" || cast.error) && !dismissed.includes(key(cast)));
  if (shown.length === 0 || route.page === "play" || route.page === "watch") return null;
  return (
    <>
      <div aria-hidden="true" className="h-28" />
      <div className="fixed inset-x-0 bottom-0 z-20 border-t border-rule bg-raised/95 pb-[env(safe-area-inset-bottom)] backdrop-blur">
        {shown.map((cast) => (
          <Remote
            key={key(cast)}
            cast={cast}
            volume={tvs.find((tv) => tv.id === cast.tv)?.volume ?? false}
            onDismiss={() => setDismissed((keys) => [...keys, key(cast)])}
          />
        ))}
      </div>
    </>
  );
}

function Remote({ cast, volume, onDismiss }: { cast: Cast; volume: boolean; onDismiss: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const [scrub, setScrub] = useState<number | null>(null);
  const [level, setLevel] = useState<number | null>(null);
  const act = (action: CastAction) => {
    setError(null);
    controlCast(cast.tv, action).then(changed, (thrown: unknown) => setError(sentence(asApiError(thrown).message)));
  };

  if (cast.state === "ended") {
    return (
      <div role="alert" className="mx-auto flex max-w-4xl items-start gap-3 px-4 py-3 sm:px-6">
        <TvIcon aria-hidden="true" className="mt-0.5 size-5 shrink-0 fill-error" />
        <div className="flex min-w-0 flex-1 flex-col">
          <p className="text-sm text-ink">
            {cast.name} stopped on {cast.tv_name}
          </p>
          {cast.error && <p className="text-sm text-pretty text-soft">{sentence(cast.error.message)}</p>}
        </div>
        <ControlButton label="Dismiss" onClick={onDismiss} className="size-8 *:[svg]:size-4">
          <XMarkIcon />
        </ControlButton>
      </div>
    );
  }

  const started = cast.state === "playing" || cast.state === "paused";
  const position = scrub ?? cast.position ?? 0;
  const status =
    cast.state === "finding" ? `Finding a source for ${cast.name}` : cast.state === "loading" ? `Starting on ${cast.tv_name}` : `On ${cast.tv_name}`;
  return (
    <section aria-label={`Playing on ${cast.tv_name}`} className="mx-auto flex max-w-4xl flex-col gap-1.5 px-4 py-2.5 sm:px-6">
      <div className="flex items-center gap-3">
        {started ? <TvIcon aria-hidden="true" className="size-5 shrink-0 fill-action" /> : <Spinner className="size-4 text-soft" />}
        <div className="flex min-w-0 flex-1 flex-col">
          <p className="truncate text-sm font-medium text-ink">{cast.name}</p>
          <p className="truncate text-xs text-soft">{status}</p>
        </div>
        {started && (
          <>
            <ControlButton label="Back 10 seconds" onClick={() => act({ action: "seek", position: Math.max(0, (cast.position ?? 0) - 10) })} className="max-sm:hidden">
              <BackwardIcon />
            </ControlButton>
            <ControlButton
              label={cast.state === "paused" ? "Resume" : "Pause"}
              onClick={() => act({ action: cast.state === "paused" ? "resume" : "pause" })}
              className="*:[svg]:size-6"
            >
              {cast.state === "paused" ? <PlayIcon /> : <PauseIcon />}
            </ControlButton>
            <ControlButton
              label="Forward 30 seconds"
              onClick={() => act({ action: "seek", position: Math.min(cast.duration ?? Infinity, (cast.position ?? 0) + 30) })}
              className="max-sm:hidden"
            >
              <ForwardIcon />
            </ControlButton>
          </>
        )}
        <ControlButton label="Stop" onClick={() => act({ action: "stop" })}>
          <StopIcon />
        </ControlButton>
      </div>
      {started && (
        <div className="flex items-center gap-3 text-xs text-soft tabular-nums">
          {cast.position !== null && <span>{clock(position)}</span>}
          {cast.duration ? (
            <input
              type="range"
              name="position"
              aria-label="Position"
              min={0}
              max={cast.duration}
              step={1}
              value={position}
              onChange={(event) => setScrub(Number(event.target.value))}
              onPointerUp={() => {
                if (scrub !== null) act({ action: "seek", position: scrub });
                setScrub(null);
              }}
              onKeyUp={() => {
                if (scrub !== null) act({ action: "seek", position: scrub });
                setScrub(null);
              }}
              className="slider min-w-0 flex-1"
              style={{ "--level": `${(position / cast.duration) * 100}%` } as CSSProperties}
            />
          ) : (
            <div className="flex-1" />
          )}
          {cast.duration && <span>{clock(cast.duration)}</span>}
          {volume && (
            <label className="flex items-center gap-1.5">
              <SpeakerWaveIcon aria-hidden="true" className="size-4 fill-soft" />
              <input
                type="range"
                name="volume"
                aria-label="TV volume"
                min={0}
                max={100}
                step={1}
                value={level ?? cast.volume ?? 0}
                onChange={(event) => setLevel(Number(event.target.value))}
                onPointerUp={() => {
                  if (level !== null) act({ action: "volume", level });
                  setLevel(null);
                }}
                onKeyUp={() => {
                  if (level !== null) act({ action: "volume", level });
                  setLevel(null);
                }}
                className="slider w-20 sm:w-28"
                style={{ "--level": `${level ?? cast.volume ?? 0}%` } as CSSProperties}
              />
            </label>
          )}
        </div>
      )}
      {error && <p className="text-xs text-pretty text-error">{error}</p>}
    </section>
  );
}
