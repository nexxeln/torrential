import { FilmIcon, TvIcon } from "@heroicons/react/24/outline";
import { type CSSProperties, type ReactNode, useCallback, useReducer, useRef, useState } from "react";
import type { Kind } from "../api";

/** Pictures loaded in this visit. One shown again, on any screen, appears at once. */
const loaded = new Set<string>();
/** Pictures the engine does not have. Their fallback shows at once instead of after asking again. */
const missing = new Set<string>();
/** A picture that arrives this soon after it is asked for came from the browser's cache, and appears without fading in. */
const CACHED_MS = 100;

interface Props {
  /** Null when the catalog has no such picture. */
  src: string | null;
  alt: string;
  className?: string;
  style?: CSSProperties;
  /** Takes the picture's place when there is none. */
  fallback?: ReactNode;
  /** Hears the picture once it has loaded, before it shows. */
  onLoad?: (image: HTMLImageElement) => void;
}

/**
 * A picture of a title. One that takes a while fades in once it has loaded, and one the browser
 * already has shows at once. The box around it holds its size, so nothing moves when it arrives
 * or when it turns out to be missing.
 */
export function Picture({ src, alt, className = "", style, fallback = null, onLoad }: Props) {
  const [, changed] = useReducer((n: number) => n + 1, 0);
  const [faded, setFaded] = useState<string | null>(null);
  const asked = useRef(0);
  const heard = useRef(onLoad);
  heard.current = onLoad;

  // A picture already decoded in this page shows before the first paint, with no fade.
  const attach = useCallback(
    (image: HTMLImageElement | null) => {
      if (!image || !src) return;
      asked.current = performance.now();
      if (!image.complete || image.naturalWidth === 0 || loaded.has(src)) return;
      heard.current?.(image);
      loaded.add(src);
      changed();
    },
    [src],
  );

  if (!src || missing.has(src)) return fallback;
  return (
    <img
      key={src}
      ref={attach}
      src={src}
      alt={alt}
      loading="lazy"
      decoding="async"
      style={style}
      onLoad={(event) => {
        heard.current?.(event.currentTarget);
        if (loaded.has(src)) return;
        loaded.add(src);
        setFaded(performance.now() - asked.current > CACHED_MS ? src : null);
        changed();
      }}
      onError={() => {
        missing.add(src);
        changed();
      }}
      data-loaded={loaded.has(src) ? "" : undefined}
      data-fade={faded === src ? "" : undefined}
      className={`opacity-0 data-loaded:opacity-100 data-fade:transition-opacity data-fade:duration-300 motion-reduce:transition-none ${className}`}
    />
  );
}

/** A plain tile in place of a picture the catalog does not have, marked with what the title is. */
export function Blank({ kind }: { kind: Kind }) {
  const Mark = kind === "film" ? FilmIcon : TvIcon;
  return (
    <div aria-hidden="true" className="absolute inset-0 flex items-center justify-center bg-linear-to-b from-raised to-white/4">
      <Mark className="size-7 stroke-1 text-white/15" />
    </div>
  );
}
