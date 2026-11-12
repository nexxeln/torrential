import { type KeyboardEvent, type Ref } from "react";
import { artUrl, type TitleCard } from "../api";
import { Link, titlePath } from "../router";
import { Picture } from "./Picture";
import { cardFacts } from "./words";

/** A title's poster, or its name on a plain tile when the catalog has none. */
export function Poster({ card }: { card: TitleCard }) {
  return (
    <div className="relative aspect-2/3 overflow-hidden rounded-lg bg-raised outline-1 -outline-offset-1 outline-white/10 group-hover/card:outline-white/30">
      <Picture
        src={artUrl(card.id, "poster")}
        alt=""
        className="absolute inset-0 size-full object-cover"
        fallback={
          <div aria-hidden="true" className="absolute inset-0 flex flex-col justify-end gap-1 bg-linear-to-b from-raised to-white/5 p-3">
            <p className="line-clamp-4 text-base font-medium text-balance text-soft sm:text-sm">{card.name}</p>
            {card.year && <p className="text-sm text-pencil tabular-nums">{card.year}</p>}
          </div>
        }
      />
    </div>
  );
}

/** Moves focus through the grid with the arrow keys, a row at a time up and down. */
function move(event: KeyboardEvent<HTMLOListElement>, onLeaveTop?: () => void): void {
  if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
  const cards = [...event.currentTarget.querySelectorAll<HTMLAnchorElement>(":scope > li > a")];
  const at = cards.indexOf(event.target as HTMLAnchorElement);
  if (at === -1) return;
  const top = cards[0]!.getBoundingClientRect().top;
  const columns = cards.filter((card) => card.getBoundingClientRect().top === top).length;
  const step = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -columns, ArrowDown: columns }[event.key];
  if (step === undefined) return;
  event.preventDefault();
  const to = at + step;
  if (to < 0) {
    if (event.key === "ArrowUp") onLeaveTop?.();
    return;
  }
  const lastRow = Math.floor((cards.length - 1) / columns);
  if (to >= cards.length && Math.floor(at / columns) === lastRow) return;
  cards[Math.min(cards.length - 1, to)]!.focus();
}

interface Props {
  titles: TitleCard[];
  label: string;
  /** Shows the titles of a search that is being replaced. */
  stale?: boolean;
  /** Up from the first row. */
  onLeaveTop?: () => void;
  list?: Ref<HTMLOListElement>;
}

/** Titles as posters. Each opens its title page. */
export function TitleGrid({ titles, label, stale = false, onLeaveTop, list }: Props) {
  return (
    <ol
      ref={list}
      role="list"
      aria-label={label}
      aria-busy={stale}
      onKeyDown={(event) => move(event, onLeaveTop)}
      className="grid grid-cols-2 gap-x-4 gap-y-6 aria-busy:opacity-50 min-[30rem]:grid-cols-3 sm:grid-cols-4 md:grid-cols-5"
    >
      {titles.map((card) => (
        <li key={card.id}>
          <Link
            href={titlePath(card.id)}
            aria-label={[card.name, cardFacts(card)].filter(Boolean).join(", ")}
            className="group/card flex flex-col gap-2 rounded-lg"
          >
            <Poster card={card} />
            <div className="flex min-w-0 flex-col px-0.5">
              <p className="line-clamp-2 text-base font-medium text-pretty text-ink sm:text-sm" title={card.name}>
                {card.name}
              </p>
              <p className="min-h-lh text-sm text-pencil tabular-nums">{cardFacts(card)}</p>
            </div>
          </Link>
        </li>
      ))}
    </ol>
  );
}
