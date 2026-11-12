import type { CSSProperties } from "react";

/** A thin bar filled to `fraction`, from 0 to 1. */
export function Meter({ fraction, label, tone = "action" }: { fraction: number; label: string; tone?: "action" | "muted" }) {
  const filled = Math.min(1, Math.max(0, fraction));
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.floor(filled * 100)}
      className="h-1 overflow-hidden rounded-full bg-white/10"
    >
      <div
        className={`h-full w-(--filled) rounded-full ${tone === "action" ? "bg-action" : "bg-pencil"}`}
        style={{ "--filled": `${filled * 100}%` } as CSSProperties}
      />
    </div>
  );
}
