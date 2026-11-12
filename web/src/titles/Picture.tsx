import { type ReactNode, useState } from "react";

/** Pictures loaded in this visit. One shown again, on another screen, appears at once instead of fading in. */
const seen = new Set<string>();

interface Props {
  /** Null when the catalog has no such picture. */
  src: string | null;
  alt: string;
  className?: string;
  /** Takes the picture's place when there is none. */
  fallback?: ReactNode;
}

/**
 * A picture of a title that fades in once it has loaded. The box around it holds its size, so
 * nothing moves when it arrives or when it turns out to be missing.
 */
export function Picture({ src, alt, className = "", fallback = null }: Props) {
  const [loaded, setLoaded] = useState<string | null>(() => (src && seen.has(src) ? src : null));
  const [missing, setMissing] = useState<string | null>(null);
  if (!src || missing === src) return fallback;
  return (
    <img
      key={src}
      src={src}
      alt={alt}
      loading="lazy"
      decoding="async"
      onLoad={() => {
        seen.add(src);
        setLoaded(src);
      }}
      onError={() => setMissing(src)}
      data-loaded={loaded === src || seen.has(src) ? "" : undefined}
      className={`opacity-0 transition-opacity duration-300 data-loaded:opacity-100 motion-reduce:transition-none ${className}`}
    />
  );
}
