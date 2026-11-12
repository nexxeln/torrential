import { type CSSProperties, type ReactNode, useState } from "react";
import { Picture } from "./Picture";

/** The part of a picture that is drawn: its edges as fractions of the picture's width and height. */
interface Ink {
  left: number;
  top: number;
  width: number;
  height: number;
  /** The drawn part's width over its height, in pixels. */
  ratio: number;
}

function whole(width: number, height: number): Ink {
  return { left: 0, top: 0, width: 1, height: 1, ratio: height > 0 ? width / height : 1 };
}
/** Alpha at or under this is an empty margin, or a glow too faint to see. */
const CLEAR = 8;

/** Logos measured in this visit. */
const inks = new Map<string, Ink>();

/**
 * Where a logo's drawn pixels lie. The catalog's logos sit centered in a fixed canvas with
 * transparent margins, so the picture's own box says nothing about where the logo starts.
 */
export function inkOf(pixels: Uint8ClampedArray, width: number, height: number): Ink {
  let [left, top, right, bottom] = [width, height, -1, -1];
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      if (pixels[(y * width + x) * 4 + 3]! <= CLEAR) continue;
      left = Math.min(left, x);
      right = Math.max(right, x);
      top = Math.min(top, y);
      bottom = Math.max(bottom, y);
    }
  }
  if (right < 0) return whole(width, height);
  const [inkWidth, inkHeight] = [right + 1 - left, bottom + 1 - top];
  return { left: left / width, top: top / height, width: inkWidth / width, height: inkHeight / height, ratio: inkWidth / inkHeight };
}

function measure(image: HTMLImageElement): Ink {
  const { naturalWidth: width, naturalHeight: height } = image;
  const canvas = document.createElement("canvas");
  [canvas.width, canvas.height] = [width, height];
  const context = canvas.getContext("2d", { willReadFrequently: true });
  if (!context || width === 0 || height === 0) return whole(width, height);
  try {
    context.drawImage(image, 0, 0);
    return inkOf(context.getImageData(0, 0, width, height).data, width, height);
  } catch {
    return whole(width, height);
  }
}

interface Props {
  src: string;
  alt: string;
  /** Takes the logo's place when the catalog has none after all. */
  fallback: ReactNode;
}

/**
 * A title's logo, framed to its drawn part so it lines up with the text beside it. It is at most
 * as tall as the box the page gives it and as wide as the column.
 */
export function Logo({ src, alt, fallback }: Props) {
  const [ink, setInk] = useState(() => inks.get(src) ?? null);
  const found = (image: HTMLImageElement) => {
    const measured = inks.get(src) ?? measure(image);
    inks.set(src, measured);
    setInk(measured);
  };
  // Until the logo is measured, the frame stays out of the layout and the picture loads unseen.
  // It is the same picture element afterwards, so it still fades in once framed.
  return (
    <div
      data-framed={ink ? "" : undefined}
      style={ink ? ({ "--ratio": ink.ratio } as CSSProperties) : undefined}
      className="contents data-framed:relative data-framed:block data-framed:aspect-(--ratio) data-framed:w-[min(100%,--spacing(96),--spacing(24)*var(--ratio))] data-framed:overflow-hidden data-framed:drop-shadow-lg sm:data-framed:w-[min(100%,--spacing(96),--spacing(32)*var(--ratio))]"
    >
      <Picture
        src={src}
        alt={alt}
        fallback={fallback}
        onLoad={found}
        className={ink ? "absolute max-w-none" : "h-24 w-auto sm:h-32"}
        style={
          ink
            ? {
                left: `${(-ink.left / ink.width) * 100}%`,
                top: `${(-ink.top / ink.height) * 100}%`,
                width: `${100 / ink.width}%`,
                height: `${100 / ink.height}%`,
              }
            : undefined
        }
      />
    </div>
  );
}
