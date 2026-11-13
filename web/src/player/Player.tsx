import { useCallback, useEffect, useState } from "react";
import { useJob } from "../lib/jobs";
import { readable } from "../lib/release";
import { goBack } from "../router";
import { Problem, Readout, Starting, TopBar } from "./Chrome";
import { leaveOnBack } from "./keys";
import { Playback } from "./Playback";
import { useSession } from "./session";

const INFOHASH = /^[0-9a-f]{40}$/;

interface Props {
  /** A release or download ID, magnet link, or infohash. */
  target: string;
  /** The file to play, or null for the engine's pick. */
  file: number | null;
}

/** The player page: starts the download, says what it is waiting for, then plays. */
export function Player({ target, file }: Props) {
  const [attempt, setAttempt] = useState(0);
  const session = useSession(target, file, attempt);
  const play = session.stage === "finding" ? null : session.play;
  const live = useJob(play?.id ?? (INFOHASH.test(target) ? target : null));
  const job = live ?? play?.job ?? null;
  const back = useCallback(() => goBack("/"), []);
  const retry = useCallback(() => setAttempt((n) => n + 1), []);

  const name = play?.name ?? job?.name ?? null;
  useEffect(() => {
    document.title = name ? `${readable(name).title} · torrential` : "torrential";
    return () => void (document.title = "torrential");
  }, [name]);

  const ready = session.stage === "ready";
  useEffect(() => {
    if (ready) return;
    const pressed = leaveOnBack(back);
    addEventListener("keydown", pressed);
    return () => removeEventListener("keydown", pressed);
  }, [ready, back]);

  return (
    <main className="fixed inset-0 bg-black text-soft">
      {session.stage === "ready" ? (
        <Playback
          key={`${session.play.id}:${session.play.video.index}`}
          play={session.play}
          media={session.media}
          onReload={retry}
          onBack={back}
        />
      ) : (
        <div className="relative size-full">
          {session.stage === "failed" ? (
            <Problem title="This video cannot start" error={session.error} onRetry={retry} onBack={back} />
          ) : (
            <Starting step={session.stage === "finding" ? 0 : 1} name={name} readout={job && <Readout job={job} />} />
          )}
          <div className="absolute inset-x-0 top-0 z-10">
            <TopBar name={session.stage === "failed" ? name : null} file={null} onBack={back} />
          </div>
        </div>
      )}
    </main>
  );
}
