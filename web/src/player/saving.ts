/** A moment the player could save where it is: its timer, a play, a pause, the end, the page hiding, or leaving the player. */
export interface Chance {
  /** A person could see the page up to now. */
  seen: boolean;
}

/** What the player saves at a chance. */
export interface Save {
  /** The video started or resumed here since the last chance, so this screen keeps its place from now on. */
  starts: boolean;
}

/**
 * Whether the player has a place to save. It has one when the video played on since the last chance, on a page a person
 * could see, or when the video started or resumed here. A start or resume takes the place from any other screen, and the
 * engine keeps a save only from the screen that started or resumed the video last. So a paused or hidden tab never writes
 * over a place that a TV or another screen kept since.
 */
export class Unsaved {
  private played = false;
  private resumed = false;

  /** The video reached a new time. A seek while paused is not playing. */
  moved(playing: boolean): void {
    if (playing) this.played = true;
  }

  /** The video started or resumed here. */
  resume(): void {
    this.resumed = true;
  }

  /** What to save at this chance, or `null` for nothing. Each chance uses up what happened before it. */
  take({ seen }: Chance): Save | null {
    const save = (this.played && seen) || this.resumed ? { starts: this.resumed } : null;
    this.played = false;
    this.resumed = false;
    return save;
  }
}
