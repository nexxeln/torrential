import { useEffect, useRef, useState } from "react";
import { addDevice, type ApiError, asApiError, type Device, type DeviceList, devices, type PairingOffer, removeDevice } from "../api";
import { age, sentence } from "../lib/format";
import { Button } from "../ui/Button";
import { Header } from "../ui/Header";

/** How often the page asks for the list, so a device that pairs shows up while the code is on screen. */
const POLL_MS = 2000;
/** Modules of light around a QR code, which a camera needs to find it. */
const QUIET = 4;

type State = { kind: "loading" } | { kind: "ready"; list: DeviceList } | { kind: "failed"; error: ApiError };

/** A QR code drawn from the engine's rows, dark on light whatever the page around it. */
function Qr({ rows, label }: { rows: string[]; label: string }) {
  const size = rows.length + 2 * QUIET;
  const path = rows
    .flatMap((row, y) => [...row].map((bit, x) => (bit === "1" ? `M${x + QUIET} ${y + QUIET}h1v1h-1z` : "")))
    .join("");
  return (
    <svg viewBox={`0 0 ${size} ${size}`} role="img" aria-label={label} shapeRendering="crispEdges" className="size-52 shrink-0 rounded-xl bg-white">
      <path d={path} fill="#000" />
    </svg>
  );
}

/** `482 913`, which reads and types more easily than six digits in a row. */
function spaced(code: string): string {
  return `${code.slice(0, 3)} ${code.slice(3)}`;
}

function remaining(seconds: number): string {
  const whole = Math.max(0, Math.ceil(seconds));
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, "0")}`;
}

function Offer({ offer, now }: { offer: PairingOffer; now: number }) {
  return (
    <section aria-label="Add a device" className="flex flex-col gap-6 rounded-2xl bg-raised p-5 sm:flex-row sm:items-center">
      <Qr rows={offer.qr} label={`QR code for ${offer.url}`} />
      <div className="flex min-w-0 flex-col gap-3 text-base text-pretty sm:text-sm">
        <p className="text-ink">Scan the code with the phone's camera.</p>
        <p>
          On a TV, open <span className="font-medium text-ink">{offer.address}</span> in its browser and enter
        </p>
        <p className="font-mono text-4xl font-medium tracking-widest text-ink tabular-nums">{spaced(offer.code)}</p>
        {offer.lan_address && offer.lan_address !== offer.address && (
          <p className="text-pencil">
            If the TV cannot open that address, use <span className="text-soft">{offer.lan_address}</span>.
          </p>
        )}
        <p className="text-pencil tabular-nums">The code works for {remaining(offer.expires_at - now)} more and lets one device in.</p>
      </div>
    </section>
  );
}

function Row({ device, now, busy, onRemove }: { device: Device; now: number; busy: boolean; onRemove: () => void }) {
  return (
    <li className="flex items-center justify-between gap-4 py-3">
      <div className="flex min-w-0 flex-col">
        <p className="truncate text-base font-medium text-ink sm:text-sm">{device.name}</p>
        <p className="text-sm text-pencil tabular-nums">
          Added {age(device.added_at, now)} ago · last seen {age(device.seen_at, now)} ago
        </p>
      </div>
      <Button variant="quiet" disabled={busy} title="Locks it out at once. Add it again with a new code." onClick={onRemove}>
        Remove
      </Button>
    </li>
  );
}

export function DevicesPage() {
  const [state, setState] = useState<State>({ kind: "loading" });
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<ApiError | null>(null);
  const [now, setNow] = useState(() => Date.now() / 1000);
  // Only the newest answer counts, so a slow poll cannot undo an add or a remove.
  const latest = useRef(0);

  const apply = (request: Promise<DeviceList>) => {
    const asked = ++latest.current;
    return request
      .then((list) => {
        if (asked === latest.current) setState({ kind: "ready", list });
      })
      .catch((thrown) => {
        if (asked === latest.current) setState({ kind: "failed", error: asApiError(thrown) });
      });
  };

  useEffect(() => {
    document.title = "Devices · torrential";
    void apply(devices());
    const clock = setInterval(() => setNow(Date.now() / 1000), 1000);
    const poll = setInterval(() => void apply(devices()), POLL_MS);
    return () => {
      clearInterval(clock);
      clearInterval(poll);
    };
  }, []);

  const act = (key: string, request: () => Promise<DeviceList>) => {
    setBusy(key);
    setProblem(null);
    const asked = ++latest.current;
    request()
      .then((list) => {
        if (asked === latest.current) setState({ kind: "ready", list });
      })
      .catch((thrown) => setProblem(asApiError(thrown)))
      .finally(() => setBusy(null));
  };

  const list = state.kind === "ready" ? state.list : null;
  const offer = list?.offer && list.offer.expires_at > now ? list.offer : null;

  return (
    <div className="mx-auto flex min-h-dvh max-w-4xl flex-col px-4 pb-16 sm:px-6">
      <Header current="devices" />
      <main className="flex flex-col gap-6 pt-[min(8vh,--spacing(16))]">
        <div className="flex flex-col gap-1">
          <h1 className="text-lg font-medium text-ink">Devices</h1>
          <p className="max-w-[60ch] text-base text-pretty text-pencil sm:text-sm">
            Phones and TVs on this network watch from this Mac once you add them. They need no account and store nothing.
          </p>
        </div>
        {state.kind === "failed" && (
          <p role="alert" className="max-w-[60ch] text-base text-pretty text-error sm:text-sm">
            {sentence(state.error.message)}
            {state.error.hint && <span className="text-pencil"> {sentence(state.error.hint)}</span>}
          </p>
        )}
        {list &&
          (offer ? (
            <Offer offer={offer} now={now} />
          ) : (
            <div className="flex flex-col items-start gap-2">
              <Button variant="primary" size="large" disabled={busy !== null} onClick={() => act("add", addDevice)}>
                Add a device
              </Button>
              <p className="text-sm text-pencil">Shows a code that lets one phone or TV in for 5 minutes.</p>
            </div>
          ))}
        {problem && (
          <p role="alert" className="text-sm text-pretty text-error">
            {sentence(problem.message)}
            {problem.hint && <span className="text-pencil"> {sentence(problem.hint)}</span>}
          </p>
        )}
        {list && (
          <section aria-label="Paired devices" className="flex flex-col">
            <h2 className="text-sm font-medium text-soft">Paired</h2>
            {list.devices.length > 0 ? (
              <ul role="list" className="divide-y divide-white/8">
                {list.devices.map((device) => (
                  <Row
                    key={device.id}
                    device={device}
                    now={now}
                    busy={busy !== null}
                    onRemove={() => act(device.id, () => removeDevice(device.id))}
                  />
                ))}
              </ul>
            ) : (
              <p className="pt-2 text-base text-pencil sm:text-sm">No phone or TV yet. Only this Mac can reach torrential.</p>
            )}
          </section>
        )}
      </main>
    </div>
  );
}
