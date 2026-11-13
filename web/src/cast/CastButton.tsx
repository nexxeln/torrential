import { ArrowPathIcon, TvIcon } from "@heroicons/react/20/solid";
import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import { type ApiError, asApiError, type Cast, type CastStart, findTvs, startCast, type Tv } from "../api";
import { sentence } from "../lib/format";
import { isBack } from "../lib/tv";
import { ControlButton } from "../player/Controls";
import { Button } from "../ui/Button";
import { Spinner } from "../ui/Spinner";
import { changed, heard, useTvList } from "./casts";

type Search = { kind: "looking" } | { kind: "done" } | { kind: "failed"; error: ApiError };

interface Props {
  /** What plays, without the TV. */
  start: Omit<CastStart, "tv">;
  /** Seconds to start the TV at, read when a TV is chosen, such as where a player here is. */
  at?: () => number | null;
  /** Called once the TV has the cast, so a player here can step aside. */
  onCast?: (cast: Cast) => void;
  /** Called as the list opens and closes, so a player keeps its controls up while it is open. */
  onOpen?: (open: boolean) => void;
  /** A large button on a page, or a round one in the player's controls. */
  look: "page" | "player";
}

function plays(tv: Tv): string {
  return tv.plays.map((container) => container.toUpperCase()).join(", ");
}

/** "Play on TV": opens a list of the TVs on the home network, and plays on the one chosen. */
export function CastButton({ start, at, onCast, onOpen, look }: Props) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const toggle = () => setOpen((was) => !was);
  /** Closes the list. Focus in it goes back to the button, so a remote keeps its place. */
  const close = () => {
    if (root.current?.querySelector("[role=dialog]")?.contains(document.activeElement)) {
      root.current.querySelector<HTMLElement>("[aria-haspopup]")?.focus();
    }
    setOpen(false);
  };
  // Back closes the list before the page or the player hears it.
  const keys = (event: KeyboardEvent) => {
    if (!open || !isBack(event.nativeEvent)) return;
    event.preventDefault();
    event.stopPropagation();
    close();
  };

  useEffect(() => onOpen?.(open), [open, onOpen]);
  useEffect(() => {
    if (!open) return;
    const outside = (event: MouseEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    addEventListener("click", outside, true);
    return () => removeEventListener("click", outside, true);
  }, [open]);

  // In the player on a phone, the list hangs from the whole bar of controls, as the button is too
  // near the left edge for its width.
  return (
    <div ref={root} className={look === "player" ? "sm:relative" : "relative"} data-menu-root onKeyDown={keys}>
      {look === "page" ? (
        <Button size="large" aria-haspopup="dialog" aria-expanded={open} onClick={toggle}>
          <TvIcon aria-hidden="true" className="size-4 fill-soft" />
          Play on TV
        </Button>
      ) : (
        <ControlButton label="Play on TV" aria-haspopup="dialog" aria-expanded={open} onClick={toggle}>
          <TvIcon />
        </ControlButton>
      )}
      {open && (
        <Picker
          start={start}
          at={at}
          above={look === "player"}
          onCast={(cast) => {
            setOpen(false);
            onCast?.(cast);
          }}
        />
      )}
    </div>
  );
}

function Picker({ start, at, above, onCast }: { start: Omit<CastStart, "tv">; at?: () => number | null; above: boolean; onCast: (cast: Cast) => void }) {
  const { tvs } = useTvList();
  const [search, setSearch] = useState<Search>({ kind: "looking" });
  const [sending, setSending] = useState<string | null>(null);
  const [error, setError] = useState<ApiError | null>(null);

  const look = (fresh: boolean) => {
    setSearch({ kind: "looking" });
    findTvs(fresh).then(
      (list) => {
        heard(list);
        setSearch({ kind: "done" });
      },
      (thrown: unknown) => setSearch({ kind: "failed", error: asApiError(thrown) }),
    );
  };
  // Asks once when the list opens. A search in the last 30 seconds answers at once.
  useEffect(() => look(false), []);

  const send = (tv: Tv) => {
    setSending(tv.id);
    setError(null);
    const startAt = at?.() ?? start.start_at;
    startCast({ ...start, tv: tv.id, start_at: startAt ?? undefined }).then(
      (cast) => {
        changed(cast);
        onCast(cast);
      },
      (thrown: unknown) => {
        setSending(null);
        setError(asApiError(thrown));
      },
    );
  };

  const looking = search.kind === "looking";
  return (
    <div
      role="dialog"
      aria-label="Play on TV"
      data-overlay=""
      className={`absolute z-30 flex w-72 max-w-[calc(100vw-2rem)] animate-pop flex-col gap-1 rounded-xl bg-raised p-1.5 shadow-xl inset-ring inset-ring-white/10 motion-reduce:animate-none ${above ? "right-(--edge-right) bottom-full mb-2 sm:right-0" : "top-full left-0 mt-2 max-sm:fixed max-sm:inset-x-4 max-sm:top-auto max-sm:bottom-[max(1rem,env(safe-area-inset-bottom))] max-sm:w-auto"}`}
    >
      <div className="flex items-center justify-between gap-2 px-2.5 pt-1.5 pb-1">
        <h2 className="text-sm font-medium text-ink">Play on TV</h2>
        <button
          type="button"
          aria-label="Look for TVs again"
          title="Look for TVs again"
          disabled={looking}
          onClick={() => look(true)}
          className="flex size-7 items-center justify-center rounded-full text-soft hover:bg-white/10 hover:text-ink disabled:opacity-50"
        >
          {looking ? <Spinner className="size-3.5" /> : <ArrowPathIcon aria-hidden="true" className="size-4 fill-current" />}
        </button>
      </div>
      {tvs.map((tv) => (
        <button
          key={tv.id}
          type="button"
          disabled={sending !== null}
          onClick={() => send(tv)}
          className="flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left hover:bg-white/10 focus-visible:bg-white/10 focus-visible:outline-none disabled:opacity-60"
        >
          <TvIcon aria-hidden="true" className="size-5 shrink-0 fill-soft" />
          <span className="flex min-w-0 flex-1 flex-col">
            <span className="truncate text-sm text-ink">{tv.name}</span>
            <span className="truncate text-xs text-pencil">
              {tv.address} · Plays {plays(tv)}
            </span>
          </span>
          {sending === tv.id && <Spinner className="size-4 text-soft" />}
        </button>
      ))}
      {tvs.length === 0 && looking && <p className="px-2.5 py-2 text-sm text-soft">Looking for TVs on this network…</p>}
      {tvs.length === 0 && search.kind === "done" && (
        <p className="px-2.5 py-2 text-sm text-pretty text-soft">
          No TV answered. Check that the TV is on, on the same network as this Mac, and that its DLNA or media sharing setting is on.
        </p>
      )}
      {search.kind === "failed" && <p className="px-2.5 py-2 text-sm text-pretty text-error">{sentence(search.error.message)}</p>}
      {error && (
        <div className="flex flex-col gap-0.5 px-2.5 py-2">
          <p className="text-sm text-pretty text-error">{sentence(error.message)}</p>
          {error.hint && <p className="text-xs text-pretty text-pencil">{sentence(error.hint)}</p>}
        </div>
      )}
    </div>
  );
}
