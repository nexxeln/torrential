// Subtitle and audio tracks name their language as an ISO 639-2 code, and subtitle files in their names.

/** ISO 639-2 codes (both forms where they differ) and English names, by ISO 639-1 code. */
const LANGUAGES: Record<string, string[]> = {
  en: ["eng", "english"],
  fr: ["fre", "fra", "french"],
  de: ["ger", "deu", "german"],
  es: ["spa", "spl", "spanish", "castellano", "latino"],
  it: ["ita", "italian"],
  pt: ["por", "pob", "portuguese", "brazilian"],
  nl: ["dut", "nld", "dutch"],
  sv: ["swe", "swedish"],
  no: ["nor", "nob", "norwegian"],
  da: ["dan", "danish"],
  fi: ["fin", "finnish"],
  pl: ["pol", "polish"],
  cs: ["cze", "ces", "czech"],
  hu: ["hun", "hungarian"],
  ro: ["rum", "ron", "romanian"],
  el: ["gre", "ell", "greek"],
  tr: ["tur", "turkish"],
  ru: ["rus", "russian"],
  uk: ["ukr", "ukrainian"],
  ar: ["ara", "arabic"],
  he: ["heb", "hebrew"],
  hi: ["hin", "hindi"],
  ja: ["jpn", "japanese"],
  ko: ["kor", "korean"],
  zh: ["chi", "zho", "zht", "zhe", "chinese", "chs", "cht"],
  th: ["tha", "thai"],
  vi: ["vie", "vietnamese"],
  id: ["ind", "indonesian"],
};

const names = new Intl.DisplayNames(["en"], { type: "language" });

/** The ISO 639-1 code for a track's language tag, or null when it is unknown or undetermined. */
export function languageCode(tag: string | null): string | null {
  const lower = tag?.toLowerCase();
  if (!lower || lower === "und") return null;
  const found = Object.entries(LANGUAGES).find(([code, aliases]) => code === lower || aliases.includes(lower));
  return found ? found[0] : null;
}

/** The language a subtitle file's name states, such as `en` for `Subs/2_English.srt` or `Show.S01E02.en.srt`. */
export function languageInPath(path: string): string | null {
  const words = path.toLowerCase().replace(/\.[a-z0-9]+$/, "").split(/[^a-z]+/);
  for (const word of words.reverse()) {
    const code = languageCode(word);
    if (code) return code;
  }
  return null;
}

export function languageName(code: string): string {
  return names.of(code) ?? code;
}

/** Subtitle sites' own codes for the regional forms of a language. */
const REGIONAL: Record<string, string> = {
  pob: "Portuguese (Brazil)",
  spl: "Spanish (Latin America)",
  zht: "Chinese (Traditional)",
  zhe: "Chinese (bilingual)",
};

/** A name for any language tag a track carries, falling back to the tag itself. */
export function languageLabel(tag: string): string {
  const code = languageCode(tag);
  return REGIONAL[tag.toLowerCase()] ?? (code ? languageName(code) : tag.toUpperCase());
}

/** The languages to try, best first: the one chosen before, then the browser's. */
export function preferredLanguages(chosen: string | null): string[] {
  const browser = navigator.languages.map((tag) => tag.slice(0, 2).toLowerCase());
  return [...new Set([...(chosen ? [chosen] : []), ...browser])];
}
