import type { ButtonHTMLAttributes } from "react";

/** A round icon button over a video or a cast, named for screen readers and on hover. */
export function ControlButton({ label, className = "", ...rest }: ButtonHTMLAttributes<HTMLButtonElement> & { label: string }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      className={`relative flex size-10 shrink-0 items-center justify-center rounded-full hover:bg-white/15 aria-expanded:bg-white/15 pointer-coarse:size-11 *:[svg]:size-5 *:[svg]:fill-ink ${className}`}
      {...rest}
    />
  );
}
