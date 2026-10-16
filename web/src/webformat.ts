/**
 * Display formatting for web analytics: timezones, durations, geography.
 * Raw codes stay in the API (groupable, stable); humans see names here.
 */

export const TZ_OPTIONS = [
  { id: "Asia/Jakarta", label: "WIB (Jakarta)" },
  { id: "UTC", label: "UTC" },
  { id: "local", label: "Browser" },
] as const;

export type TzId = (typeof TZ_OPTIONS)[number]["id"];

export function loadTz(): TzId {
  try {
    const v = localStorage.getItem("cl-web-tz");
    if (v === "UTC" || v === "local" || v === "Asia/Jakarta") return v;
  } catch {
    /* private mode */
  }
  return "Asia/Jakarta";
}

export function saveTz(tz: TzId): void {
  try {
    localStorage.setItem("cl-web-tz", tz);
  } catch {
    /* ignore */
  }
}

/** IANA zone for Intl; "local" means the browser default (undefined). */
export function tzZone(tz: TzId): string | undefined {
  return tz === "local" ? undefined : tz;
}

/** Format an ISO timestamp in the selected zone, compact + unambiguous. */
export function fmtTs(iso: string, tz: TzId): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  try {
    return new Intl.DateTimeFormat("en-GB", {
      timeZone: tzZone(tz),
      day: "2-digit",
      month: "short",
      hour: "2-digit",
      minute: "2-digit",
      hour12: false,
    }).format(d);
  } catch {
    return d.toLocaleString();
  }
}

/** Milliseconds of offset of `tz` at instant `t` (for tick alignment). */
export function tzOffsetMs(tz: TzId, t: number): number {
  if (tz === "UTC") return 0;
  if (tz === "local") return -new Date(t).getTimezoneOffset() * 60_000;
  try {
    const dtf = new Intl.DateTimeFormat("en-US", {
      timeZone: tz,
      hour12: false,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
    const parts = Object.fromEntries(
      dtf.formatToParts(new Date(t)).map((p) => [p.type, p.value]),
    );
    const asUTC = Date.UTC(
      +parts["year"],
      +parts["month"] - 1,
      +parts["day"],
      +parts["hour"] % 24,
      +parts["minute"],
      +parts["second"],
    );
    return asUTC - t;
  } catch {
    return 0;
  }
}

/** Durations: ms → "45s" / "1.5 min" / "2.3 h" (never millions of bare ms). */
export function fmtDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)}s`;
  if (ms < 3_600_000) return `${(ms / 60_000).toFixed(1)} min`;
  return `${(ms / 3_600_000).toFixed(1)} h`;
}

/** "ID" → "Indonesia" via the platform's own CLDR data. */
export function countryName(code: string): string {
  if (!code || code === "unknown" || code === "(none)") return code || "Unknown";
  try {
    const names = new Intl.DisplayNames(["en"], { type: "region" });
    return names.of(code.toUpperCase()) ?? code;
  } catch {
    return code;
  }
}

/** Capitalize a stored label for display ("mobile" → "Mobile"). */
export function cap(s: string): string {
  return s ? s.charAt(0).toUpperCase() + s.slice(1) : s;
}

/** "24h" / "7d" / "90d" / "5m" … → milliseconds (for client-side ranges). */
export function parseWindowMs(w: string): number {
  const m = /^(\d+)([smhd])$/.exec(w.trim());
  if (!m) return 3_600_000;
  const n = Number(m[1]);
  switch (m[2]) {
    case "m": return n * 60_000;
    case "h": return n * 3_600_000;
    case "d": return n * 86_400_000;
    default: return n * 1000;
  }
}

/** Largest-remainder rounding: shares that always total exactly 100. */
export function largestRemainder(values: number[]): number[] {
  const total = values.reduce((a, b) => a + b, 0);
  if (total <= 0) return values.map(() => 0);
  const exact = values.map((v) => (v / total) * 100);
  const floors = exact.map((v) => Math.floor(v));
  let rest = 100 - floors.reduce((a, b) => a + b, 0);
  const order = exact
    .map((v, i) => ({ i, frac: v - Math.floor(v) }))
    .sort((a, b) => b.frac - a.frac);
  const out = [...floors];
  for (const { i } of order) {
    if (rest <= 0) break;
    out[i] += 1;
    rest -= 1;
  }
  return out;
}

/** Indonesian province names for MaxMind ISO subdivision codes.
/// Covers the high-traffic provinces; anything else falls back to the code
/// (better a code than a wrong name).
 */
const ID_PROVINCES: Record<string, string> = {
  AC: "Aceh",
  SU: "Sumatera Utara",
  SB: "Sumatera Barat",
  RI: "Riau",
  JA: "Jambi",
  SS: "Sumatera Selatan",
  BE: "Bengkulu",
  LA: "Lampung",
  BB: "Kep. Bangka Belitung",
  KR: "Kep. Riau",
  JK: "Jakarta",
  JB: "Jawa Barat",
  JT: "Jawa Tengah",
  YO: "DI Yogyakarta",
  JI: "Jawa Timur",
  BT: "Banten",
  BA: "Bali",
  NB: "NTB",
  NT: "NTT",
  GO: "Gorontalo",
  MA: "Maluku",
  MU: "Maluku Utara",
  PA: "Papua",
  PB: "Papua Barat",
};

/** Region display: "Jawa Barat" for known ID provinces, else the raw code.
 * The ID map applies when the country is Indonesia *or* unknown (region
 * rows carry no country context; for ID-centric traffic an unknown "JB" is
 * overwhelmingly Jawa Barat — adjust if your traffic disagrees).
 */
export function regionName(code: string, country?: string | null): string {
  if (!code || code === "unknown" || code === "(none)") return "Unknown";
  if (country === "ID" || (!country && /^[A-Z]{2}$/.test(code))) {
    const hit = ID_PROVINCES[code.toUpperCase()];
    if (hit) return hit;
  }
  return code;
}

/** Screen width → device-class label with resolution. */
export function screenLabel(w: string): string {
  if (!w || w === "unknown" || w === "(none)") return "Unknown";
  const n = Number(w);
  if (!Number.isFinite(n)) return w;
  const cls = n < 768 ? "Mobile" : n < 1100 ? "Tablet" : "Desktop";
  return `${cls} · ${n}px`;
}
