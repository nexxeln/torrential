export function Spinner({ className = "size-5" }: { className?: string }) {
  return (
    <div
      role="status"
      aria-label="Loading"
      className={`${className} shrink-0 animate-spin rounded-full border-2 border-current/20 border-t-current motion-reduce:animate-[spin_3s_linear_infinite]`}
    />
  );
}
