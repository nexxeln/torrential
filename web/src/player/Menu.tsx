import { CheckIcon, ChevronLeftIcon, ChevronRightIcon } from "@heroicons/react/16/solid";
import { type CSSProperties, type KeyboardEvent, type ReactNode, useEffect, useRef } from "react";

const ITEMS = "[data-item]:not(:disabled)";
const ROW = "flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left text-sm text-ink hover:bg-white/10 focus-visible:bg-white/10 focus-visible:outline-none disabled:text-pencil disabled:hover:bg-transparent";

interface MenuProps {
  /** Names the page for screen readers and keys the page's entrance. */
  page: string;
  title: string;
  /** Goes up one page. Absent on the first page. */
  onBack?: () => void;
  onClose: () => void;
  children: ReactNode;
}

/**
 * One page of the settings popover. Up and Down move through its rows, Right or Enter opens a
 * row that leads on, Left or Backspace goes back a page, and Escape goes back or closes.
 */
export function Menu({ page, title, onBack, onClose, children }: MenuProps) {
  const menu = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const items = [...menu.current!.querySelectorAll<HTMLElement>(ITEMS)];
    (items.find((item) => item.getAttribute("aria-checked") === "true") ?? items[0])?.focus();
  }, [page]);

  useEffect(() => {
    // Capture, so a click on the video closes the menu before the video hears it as play or pause.
    const outside = (event: MouseEvent) => {
      if (!(event.target as Element).closest("[data-menu-root]")) onClose();
    };
    addEventListener("click", outside, true);
    return () => removeEventListener("click", outside, true);
  }, [onClose]);

  const keys = (event: KeyboardEvent) => {
    const items = [...menu.current!.querySelectorAll<HTMLElement>(ITEMS)];
    const focused = document.activeElement as HTMLElement;
    const index = items.indexOf(focused);
    const slider = focused instanceof HTMLInputElement;
    const handled = () => {
      event.preventDefault();
      event.stopPropagation();
    };
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp":
        handled();
        return items[(index + (event.key === "ArrowDown" ? 1 : -1) + items.length) % items.length]?.focus();
      case "Home":
      case "End":
        handled();
        return items.at(event.key === "Home" ? 0 : -1)?.focus();
      case "ArrowRight":
        if (slider || !focused.hasAttribute("data-leads")) return;
        handled();
        return focused.click();
      case "ArrowLeft":
      case "Backspace":
        if (slider || !onBack) return;
        handled();
        return onBack();
      case "Escape":
        handled();
        return onBack ? onBack() : onClose();
    }
  };

  return (
    <div
      ref={menu}
      role="menu"
      aria-label={title}
      onKeyDown={keys}
      className="absolute right-0 bottom-full z-20 mb-3 max-h-[min(78vh,--spacing(144))] w-80 max-w-[calc(100vw-(--spacing(6)))] animate-pop overflow-x-hidden overflow-y-auto overscroll-contain rounded-2xl bg-neutral-900/85 p-1.5 inset-ring inset-ring-white/10 backdrop-blur-xl motion-reduce:animate-none"
    >
      <div key={page} className="animate-page motion-reduce:animate-none" style={{ "--page-from": onBack ? "10px" : "-10px" } as CSSProperties}>
        {onBack ? (
          <button type="button" role="menuitem" data-back="" onClick={onBack} className={`${ROW} font-medium`}>
            <ChevronLeftIcon className="size-4 shrink-0 fill-soft" />
            {title}
          </button>
        ) : (
          <div className="px-2.5 pt-2 pb-1.5 text-xs font-medium text-pencil">{title}</div>
        )}
        {children}
      </div>
    </div>
  );
}

interface LinkProps {
  label: string;
  /** The row's current setting. */
  value?: string;
  onOpen: () => void;
}

/** A row that leads to another page. */
export function MenuLink({ label, value, onOpen }: LinkProps) {
  return (
    <button type="button" role="menuitem" data-item="" data-leads="" onClick={onOpen} className={ROW}>
      <span className="flex-1">{label}</span>
      {value && <span className="max-w-36 truncate text-soft">{value}</span>}
      <ChevronRightIcon className="size-4 shrink-0 fill-pencil" />
    </button>
  );
}

interface ChoiceProps {
  checked: boolean;
  disabled?: boolean;
  detail?: string | null;
  /** A short mark at the end of the row, such as where the track comes from. */
  mark?: ReactNode;
  onSelect: () => void;
  children: ReactNode;
}

/** One of a set of choices, with a tick on the one in effect. */
export function MenuChoice({ checked, disabled, detail, mark, onSelect, children }: ChoiceProps) {
  return (
    <button
      type="button"
      role="menuitemradio"
      aria-checked={checked}
      data-item=""
      disabled={disabled}
      onClick={onSelect}
      className={`group/choice ${ROW} items-start`}
    >
      <CheckIcon className="size-4 h-lh shrink-0 fill-action group-aria-[checked=false]/choice:invisible" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <div className="min-w-0 flex-1 truncate">{children}</div>
          {mark}
        </div>
        {detail && <div className="truncate text-xs text-pencil">{detail}</div>}
      </div>
    </button>
  );
}

export function MenuSection({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div role="group" aria-label={title} className="mt-1 border-t border-white/8 pt-1">
      <div className="px-2.5 pt-1.5 pb-1 text-xs font-medium text-pencil">{title}</div>
      {children}
    </div>
  );
}

/** A quiet line: what is loading, or that there is nothing. */
export function MenuNote({ children }: { children: ReactNode }) {
  return <p className="px-2.5 py-1.5 text-xs text-pretty text-pencil">{children}</p>;
}

/** A small button inside a row, such as the minus and plus of a stepper. */
export function MenuButton({ label, onPress, children, pressed }: { label: string; onPress: () => void; children: ReactNode; pressed?: boolean }) {
  return (
    <button
      type="button"
      data-item=""
      aria-label={label}
      title={label}
      aria-pressed={pressed}
      onClick={onPress}
      className="flex h-7 min-w-7 items-center justify-center rounded-md px-2 text-sm font-medium text-ink inset-ring inset-ring-white/10 hover:bg-white/10 focus-visible:bg-white/15 focus-visible:outline-none aria-pressed:bg-white/15 aria-pressed:inset-ring-white/25"
    >
      {children}
    </button>
  );
}

/** A labelled row holding small controls. */
export function MenuRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div role="group" aria-label={label} className="flex items-center gap-2 px-2.5 py-1.5 text-sm text-ink">
      <span className="flex-1">{label}</span>
      <div className="flex items-center gap-1">{children}</div>
    </div>
  );
}
