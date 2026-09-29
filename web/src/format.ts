// How numbers, times and codes read on the dashboard.

const TZ = "Asia/Jerusalem";

export const OUTCOMES: Record<string, string> = {
  CallerHungUp: "המתקשר ניתק",
  AgentHungUp: "הסתיימה",
  HandedOff: "הועבר למוקדן",
  TimeLimit: "מגבלת זמן",
};

export const ACTIONS: Record<string, string> = {
  read_back: "הקראה",
  submit: "שליחה",
  transfer: "העברה",
  end_call: "סיום",
};

export const SLOTS: Record<string, string> = {
  pickup: "איסוף",
  destination: "יעד",
  passengers: "נוסעים",
  pickup_time: "שעה",
  customer_name: "שם",
  notes: "הערה",
  luggage: "מזוודות",
};

export function dateTime(iso: string): string {
  return new Date(iso).toLocaleString("he-IL", { timeZone: TZ, day: "numeric", month: "numeric", year: "numeric", hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
}

export function clock(iso: string): string {
  return new Date(iso).toLocaleTimeString("he-IL", { timeZone: TZ, hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

export function hhmm(iso: string): string {
  return new Date(iso).toLocaleTimeString("he-IL", { timeZone: TZ, hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
}

export function dayLabel(day: string): string {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d, 12)).toLocaleDateString("he-IL", { timeZone: TZ, day: "numeric", month: "numeric" });
}

/** "יום שלישי, 29 בספטמבר" */
export function longDate(d: Date = new Date()): string {
  return d.toLocaleDateString("he-IL", { timeZone: TZ, weekday: "long", day: "numeric", month: "long" });
}

/** The greeting for the hour in Israel. */
export function greeting(d: Date = new Date()): string {
  const h = Number(d.toLocaleTimeString("en-GB", { timeZone: TZ, hour: "2-digit", hourCycle: "h23" }).slice(0, 2));
  if (h < 5) return "לילה טוב";
  if (h < 12) return "בוקר טוב";
  if (h < 18) return "צהריים טובים";
  if (h < 22) return "ערב טוב";
  return "לילה טוב";
}

/** "עכשיו", "לפני 5 דקות", "לפני שעתיים", "אתמול ב-14:11", else the date. */
export function ago(iso: string, now: number = Date.now()): string {
  const t = new Date(iso).getTime();
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 45) return "עכשיו";
  const m = Math.round(s / 60);
  if (m < 60) return m === 1 ? "לפני דקה" : `לפני ${m} דקות`;
  const h = Math.round(m / 60);
  if (h < 6) return h === 1 ? "לפני שעה" : h === 2 ? "לפני שעתיים" : `לפני ${h} שעות`;
  const day = (x: number) => new Date(x).toLocaleDateString("en-CA", { timeZone: TZ });
  const today = day(now);
  if (day(t) === today) return `היום ב-${hhmm(iso)}`;
  if (day(t) === day(now - 86_400_000)) return `אתמול ב-${hhmm(iso)}`;
  return new Date(iso).toLocaleDateString("he-IL", { timeZone: TZ, day: "numeric", month: "numeric" }) + ` · ${hhmm(iso)}`;
}

/** Milliseconds as seconds: "1.18 ש׳" reads the same way in Hebrew and in numbers. */
export function secondsOf(ms: number): string {
  return `${(ms / 1000).toFixed(2)} ש׳`;
}

export function pct(x: number | null | undefined): string {
  return x == null ? "—" : `${Math.round(x * 100)}%`;
}

export function dollars(x: number | null | undefined): string {
  if (x == null) return "—";
  return `$${x < 0.1 ? x.toFixed(4) : x.toFixed(2)}`;
}

export function duration(s: number | null | undefined): string {
  if (s == null) return "—";
  if (s < 60) return `${Math.round(s)} ש׳`;
  const m = Math.floor(s / 60);
  return `${m}:${String(Math.round(s % 60)).padStart(2, "0")}`;
}

export function tokens(n: number): string {
  return n >= 1000 ? `${Math.round(n / 1000)}K` : String(n);
}

export function phone(p: string | null | undefined): string {
  if (!p) return "—";
  // +972545460223 → 054-546-0223
  const m = /^\+972(\d{2})(\d{3})(\d{4})$/.exec(p);
  return m ? `0${m[1]}-${m[2]}-${m[3]}` : p;
}

/** The days of the last `days`, oldest first, in Israel's calendar. */
export function lastDays(days: number): string[] {
  const out: string[] = [];
  const today = new Date();
  for (let i = days - 1; i >= 0; i--) {
    const d = new Date(today.getTime() - i * 86_400_000);
    out.push(d.toLocaleDateString("en-CA", { timeZone: TZ }));
  }
  return out;
}

/** The change from `before` to `now`, as a fraction (null when there was nothing before). */
export function change(now: number, before: number): number | null {
  return before > 0 ? (now - before) / before : null;
}
