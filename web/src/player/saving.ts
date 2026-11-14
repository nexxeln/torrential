/** A moment the player could save where it is: its timer, a pause, the end, the page hiding, or leaving the player. */
export interface Chance {
  /** A person could see the page up to now. */
  seen: boolean;
  /** A TV plays the same video now. */
  cast: boolean;
}

/**
 * Whether the player has a place worth saving. It has one only when the video played on since the
 * last chance, on a page a person could see, while no TV played the same video. So a paused or
 * hidden tab never writes over a place that a TV or another screen kept since.
 */
export class Unsaved {
  private played = false;

  /** The video reached a new time. A seek while paused is not playing. */
  moved(playing: boolean): void {
    if (playing) this.played = true;
  }

  /** Whether to save at this chance. Each chance uses up what played before it. */
  take({ seen, cast }: Chance): boolean {
    const due = this.played && seen && !cast;
    this.played = false;
    return due;
  }
}
