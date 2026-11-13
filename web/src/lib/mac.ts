import { useEffect, useState } from "react";
import { me } from "../api";

let asked: Promise<boolean> | null = null;

/** True in a browser on the Mac that runs the engine, false on a paired phone or TV. The engine is asked once. */
export function useOnMac(): boolean {
  const [mac, setMac] = useState(false);
  useEffect(() => {
    let live = true;
    asked ??= me()
      .then((answer) => answer.asker === "mac")
      .catch(() => {
        asked = null;
        return false;
      });
    void asked.then((answer) => live && setMac(answer));
    return () => {
      live = false;
    };
  }, []);
  return mac;
}
