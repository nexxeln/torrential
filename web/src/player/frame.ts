import { useEffect, useRef } from "react";

// One animation-frame loop for everything that follows the playhead: the seek bar, the time, the subtitles.
const draws = new Set<() => void>();
let frame = 0;

function tick(): void {
  for (const draw of draws) draw();
  frame = requestAnimationFrame(tick);
}

/** Calls `draw` before every repaint, without rendering React. Returns a function that stops. */
function onFrame(draw: () => void): () => void {
  draws.add(draw);
  if (draws.size === 1) frame = requestAnimationFrame(tick);
  return () => {
    draws.delete(draw);
    if (draws.size === 0) cancelAnimationFrame(frame);
  };
}

/** Runs the latest `draw` before every repaint while the component is mounted. */
export function useFrame(draw: () => void): void {
  const latest = useRef(draw);
  latest.current = draw;
  useEffect(() => onFrame(() => latest.current()), []);
}
