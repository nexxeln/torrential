import { XMarkIcon } from "@heroicons/react/16/solid";
import { useEffect, useRef } from "react";
import { SHORTCUTS } from "./keys";

/** Every keyboard shortcut of the player, behind `?`. */
export function Shortcuts({ onClose }: { onClose: () => void }) {
  const close = useRef<HTMLButtonElement>(null);
  useEffect(() => close.current?.focus(), []);

  return (
    <div data-overlay="" className="absolute inset-0 z-30 flex items-center justify-center bg-black/60 p-4" onClick={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="shortcuts-title"
        onClick={(event) => event.stopPropagation()}
        className="flex max-h-full w-full max-w-3xl animate-pop flex-col overflow-hidden rounded-2xl bg-neutral-900/90 inset-ring inset-ring-white/10 backdrop-blur-xl motion-reduce:animate-none"
      >
        <div className="flex items-center justify-between py-3 pr-3 pl-6">
          <h2 id="shortcuts-title" className="text-base font-medium text-ink">
            Keyboard shortcuts
          </h2>
          <button
            ref={close}
            type="button"
            aria-label="Close"
            onClick={onClose}
            className="relative flex size-8 items-center justify-center rounded-lg hover:bg-white/10"
          >
            <XMarkIcon className="size-4 fill-soft" />
          </button>
        </div>
        <div className="grid gap-x-10 gap-y-6 overflow-y-auto px-6 pb-6 sm:grid-cols-2">
          {SHORTCUTS.map((group) => (
            <section key={group.title} aria-label={group.title} className="flex flex-col gap-1 first:row-span-2">
              <h3 className="pb-1 text-xs font-medium text-pencil">{group.title}</h3>
              <dl className="flex flex-col">
                {group.keys.map((key) => (
                  <div key={key.keys} className="flex items-baseline justify-between gap-4 border-t border-white/8 py-1.5 text-sm first:border-t-0">
                    <dt className="text-soft">{key.does}</dt>
                    <dd className="shrink-0 font-medium whitespace-pre text-ink">
                      <kbd className="font-sans">{key.keys}</kbd>
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
        </div>
      </div>
    </div>
  );
}
