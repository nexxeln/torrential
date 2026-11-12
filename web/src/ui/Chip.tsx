import type { ReactNode } from "react";

const TONES = {
  plain: "text-soft inset-ring-white/10",
  good: "text-action inset-ring-action/30",
  bad: "text-error inset-ring-error/30",
};

export function Chip({ tone = "plain", title, children }: { tone?: keyof typeof TONES; title?: string; children: ReactNode }) {
  return (
    <span title={title} className={`rounded-md px-1.5 py-0.5 text-xs font-medium whitespace-nowrap inset-ring ${TONES[tone]}`}>
      {children}
    </span>
  );
}
