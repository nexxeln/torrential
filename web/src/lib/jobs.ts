import { useSyncExternalStore } from "react";
import { type Job, type JobEvent, watchJobs } from "../api";

export interface JobsView {
  /** `lost` while the engine cannot be reached. The browser keeps trying, and a snapshot follows when it answers. */
  link: "connecting" | "live" | "lost";
  /** Newest first. */
  jobs: Job[];
}

// One event stream for the whole app, open while any component reads jobs.
let view: JobsView = { link: "connecting", jobs: [] };
let stop: (() => void) | null = null;
const listeners = new Set<() => void>();

function publish(next: JobsView): void {
  view = next;
  listeners.forEach((listener) => listener());
}

function newestFirst(jobs: Job[]): Job[] {
  return [...jobs].sort((a, b) => b.added_at - a.added_at || a.id.localeCompare(b.id));
}

function apply(event: JobEvent): void {
  switch (event.event) {
    case "snapshot":
      return publish({ link: "live", jobs: newestFirst(event.jobs) });
    case "job": {
      const others = view.jobs.filter((job) => job.id !== event.job.id);
      return publish({ link: "live", jobs: newestFirst([...others, event.job]) });
    }
    case "removed":
      return publish({ link: "live", jobs: view.jobs.filter((job) => job.id !== event.id) });
    case "heartbeat":
      if (view.link !== "live") publish({ ...view, link: "live" });
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  stop ??= watchJobs(apply, (connected) => {
    // An open stream is live once its snapshot arrives, which `apply` records.
    if (!connected && view.link !== "lost") publish({ ...view, link: "lost" });
  });
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) {
      stop?.();
      stop = null;
      view = { ...view, link: "connecting" };
    }
  };
}

export function useJobs(): JobsView {
  return useSyncExternalStore(subscribe, () => view);
}

function find(id: string | null): Job | null {
  if (!id) return null;
  return view.jobs.find((job) => job.id === id) ?? view.jobs.find((job) => job.id.startsWith(id)) ?? null;
}

/**
 * The download with this ID, or whose ID starts with it. A job keeps its object until an event
 * changes it, so news about other downloads renders nothing here.
 */
export function useJob(id: string | null): Job | null {
  return useSyncExternalStore(subscribe, () => find(id));
}

export function isActive(job: Job): boolean {
  return job.state === "downloading" || job.state === "metadata" || job.state === "checking" || job.state === "queued";
}
