import type { ButtonHTMLAttributes } from "react";

const STYLES = {
  /** The one main action of a screen. */
  primary: "bg-ink text-paper hover:bg-white",
  secondary: "text-ink inset-ring inset-ring-white/15 hover:bg-white/10",
  quiet: "text-soft hover:bg-white/10 hover:text-ink",
};

const SIZES = {
  /** Inside a line of text, such as a notice over the picture. Its hit area is larger than it on a touchscreen. */
  small: "relative hit-area px-2 py-1 text-sm",
  medium: "px-3 py-1.5 text-sm pointer-coarse:min-h-11",
  /** A page's main actions. */
  large: "px-4 py-2.5 text-base sm:py-2 sm:text-sm pointer-coarse:min-h-11",
};

export type ButtonStyle = { variant?: keyof typeof STYLES; size?: keyof typeof SIZES };

/** The classes of a button, for a link that looks like one. */
export function buttonClass({ variant = "secondary", size = "medium" }: ButtonStyle): string {
  return `inline-flex items-center justify-center gap-1.5 rounded-lg font-medium disabled:opacity-50 ${SIZES[size]} ${STYLES[variant]}`;
}

interface Props extends ButtonHTMLAttributes<HTMLButtonElement>, ButtonStyle {}

export function Button({ variant, size, className = "", ...rest }: Props) {
  return <button type="button" className={`${buttonClass({ variant, size })} ${className}`} {...rest} />;
}
