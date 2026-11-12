import type { ButtonHTMLAttributes } from "react";

const STYLES = {
  /** The one main action of a screen. */
  primary: "bg-ink text-paper hover:bg-white",
  secondary: "text-ink inset-ring inset-ring-white/15 hover:bg-white/10",
  quiet: "text-soft hover:bg-white/10 hover:text-ink",
};

interface Props extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: keyof typeof STYLES;
}

export function Button({ variant = "secondary", className = "", ...rest }: Props) {
  return (
    <button
      type="button"
      className={`inline-flex items-center justify-center gap-1.5 rounded-lg px-3 py-1.5 text-sm font-medium disabled:opacity-50 ${STYLES[variant]} ${className}`}
      {...rest}
    />
  );
}
