import { MagnifyingGlassIcon, XMarkIcon } from "@heroicons/react/16/solid";
import { type KeyboardEvent, type ReactNode, type RefObject, useEffect, useRef, useState } from "react";
import { navigate, watchPath } from "../router";
import { Spinner } from "../ui/Spinner";

/** Typing pauses this long before a search starts. */
const TYPING_MS = 300;
/** Text that names one torrent instead of describing one: a magnet link, an infohash, or a .torrent URL. */
const TORRENT = /^(magnet:\?\S+|[0-9a-f]{40}|https?:\/\/\S+\.torrent(\?\S*)?)$/i;

export interface SearchText {
  text: string;
  setText: (text: string) => void;
  /** The text names one torrent, which Enter plays. */
  torrent: boolean;
  commit: (words: string) => void;
  /** Escape clears, and Enter plays a torrent or searches before the typing pause ends. True when the key was used. */
  keyDown: (event: KeyboardEvent<HTMLInputElement>) => boolean;
}

/**
 * The text in a search field, given to `onCommit` after a typing pause. `query` is the search in
 * the address bar, which back and forward change from outside the field.
 */
export function useSearchText(query: string, onCommit: (words: string) => void): SearchText {
  const [text, setText] = useState(query);
  const committed = useRef(query);

  useEffect(() => {
    if (query !== committed.current) {
      committed.current = query;
      setText(query);
    }
  }, [query]);

  const commit = (words: string) => {
    committed.current = words;
    onCommit(words);
  };
  const torrent = TORRENT.test(text.trim());
  useEffect(() => {
    if (torrent || text.trim() === committed.current.trim()) return;
    const timer = setTimeout(() => commit(text.trim()), TYPING_MS);
    return () => clearTimeout(timer);
  });

  const keyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Escape" && text) {
      event.preventDefault();
      setText("");
      commit("");
    } else if (event.key === "Enter" && torrent) {
      event.stopPropagation();
      navigate(watchPath(text.trim()));
    } else if (event.key === "Enter" && text.trim() !== committed.current.trim()) {
      // Enter before the typing pause ends searches now, instead of opening a result of the previous query.
      event.stopPropagation();
      commit(text.trim());
    } else {
      return false;
    }
    return true;
  };

  return { text, setText, torrent, commit, keyDown };
}

interface Props {
  input: RefObject<HTMLInputElement | null>;
  search: SearchText;
  label: string;
  placeholder: string;
  busy: boolean;
  onKeyDown: (event: KeyboardEvent<HTMLInputElement>) => void;
  /** The line under the field, which screen readers announce. */
  status: ReactNode;
  /** At the right of the status line. */
  aside?: ReactNode;
}

/** The search field at the top of a page, which `/` focuses from anywhere on it. */
export function SearchField({ input, search, label, placeholder, busy, onKeyDown, status, aside }: Props) {
  const { text, setText, commit } = search;
  useEffect(() => {
    const pressed = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "/" || event.metaKey || event.ctrlKey || event.altKey) return;
      if ((event.target as Element).closest("input, textarea, select")) return;
      event.preventDefault();
      input.current?.focus();
      input.current?.select();
    };
    addEventListener("keydown", pressed);
    return () => removeEventListener("keydown", pressed);
  }, [input]);

  return (
    <search className="sticky top-0 z-10 -mx-1 flex flex-col gap-2 bg-paper px-1 pt-3 pb-2">
      <div className="grid grid-cols-[--spacing(11)_1fr_auto] items-center">
        <input
          ref={input}
          type="search"
          name="q"
          aria-label={label}
          placeholder={placeholder}
          autoFocus
          autoComplete="off"
          spellCheck={false}
          enterKeyHint="search"
          value={text}
          onChange={(event) => setText(event.target.value)}
          onKeyDown={onKeyDown}
          className="col-span-full row-start-1 h-12 w-full appearance-none rounded-xl bg-raised pr-12 pl-11 text-base text-ink inset-ring inset-ring-white/10 placeholder:text-pencil focus:outline-2 focus:-outline-offset-1 focus:outline-action/70 [&::-webkit-search-cancel-button]:hidden"
        />
        <div className="pointer-events-none col-start-1 row-start-1 flex justify-center">
          {busy ? <Spinner className="size-4 text-soft" /> : <MagnifyingGlassIcon className="size-4 fill-pencil" />}
        </div>
        <div className="col-start-3 row-start-1 flex items-center pr-2">
          {text ? (
            <button
              type="button"
              aria-label="Clear the search"
              onClick={() => (setText(""), commit(""), input.current?.focus())}
              className="relative flex size-8 items-center justify-center rounded-lg hover:bg-white/10"
            >
              <XMarkIcon className="size-4 fill-soft" />
              <span className="absolute top-1/2 left-1/2 size-[max(100%,3rem)] -translate-1/2 pointer-fine:hidden" aria-hidden="true" />
            </button>
          ) : (
            <kbd className="pointer-events-none mr-1.5 rounded-md px-1.5 py-0.5 font-sans text-xs font-medium text-pencil inset-ring inset-ring-white/10 max-sm:hidden">
              /
            </kbd>
          )}
        </div>
      </div>
      <div className="flex min-h-7 items-center justify-between gap-4 px-0.5">
        <p aria-live="polite" className="truncate text-sm text-pencil tabular-nums">
          {search.torrent ? "Press Enter to play this torrent" : status}
        </p>
        {aside}
      </div>
    </search>
  );
}
