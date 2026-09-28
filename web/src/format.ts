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

export function dayLabel(day: string): string {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d, 12)).toLocaleDateString("he-IL", { timeZone: TZ, day: "numeric", month: "numeric" });
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
