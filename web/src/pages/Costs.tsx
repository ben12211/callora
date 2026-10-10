// What each call cost and the average of the latest ones: the phone line, speech recognition,
// the agent's model, the second hearings and the live voice, priced with rates the owner can
// correct (they change, and plans differ).

import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { ArrowLeft } from "lucide-react";
import { Unauthorized, useApi } from "../api";
import { duration, phone } from "../format";
import { Ago, Button, Card, Empty, Input, PageHeader, Problem, Segmented, Skeleton, SURFACE, cx, useToast } from "../ui";

type Rates = {
  phone_per_minute: number;
  stream_per_minute: number;
  stt_per_minute: number;
  llm_input_per_million: number;
  llm_cached_per_million: number;
  llm_output_per_million: number;
  second_hearing_each: number;
  tts_per_thousand_chars: number;
  monthly_fixed: number;
  shekels_per_dollar: number;
};
type Cost = {
  phone: number | null;
  stream: number | null;
  stt: number | null;
  llm: number;
  second_hearing: number;
  tts: number | null;
  total: number;
  second_hearings: number;
  tts_chars: number | null;
  unknown: string[];
};
type Row = { id: string; from: string | null; started_at: string; outcome: string | null; seconds: number | null; orders: number; cost: Cost };
type Part = "phone" | "stream" | "stt" | "llm" | "second_hearing" | "tts";
type Costs = {
  rates: Rates;
  defaults: Rates;
  calls: Row[];
  summary: {
    calls: number;
    minutes: number;
    total: number;
    average: Record<Part | "total", number>;
    per_minute: number | null;
    calls_last_30_days: number;
    fixed_per_call: number | null;
  };
};

const PARTS: { key: Part; label: string; color: string; what: string }[] = [
  { key: "phone", label: "קו טלפון", color: "#6b66f2", what: "טוויליו, לפי דקה שהתחילה" },
  { key: "stream", label: "העברת הקול לשרת", color: "#a5a1f8", what: "טוויליו Media Streams" },
  { key: "stt", label: "זיהוי דיבור", color: "#10b981", what: "Soniox, לפי אורך השיחה" },
  { key: "llm", label: "המודל", color: "#f59e0b", what: "gpt-6-luna, לפי טוקנים" },
  { key: "second_hearing", label: "שמיעה שנייה", color: "#f43f5e", what: "gpt-audio, לכל פעם" },
  { key: "tts", label: "קול חי", color: "#0ea5e9", what: "ElevenLabs, לפי תווים (משפטים מוקלטים לא עולים)" },
];

const RATE_FIELDS: { key: keyof Rates; label: string; unit: string }[] = [
  { key: "phone_per_minute", label: "קו טלפון", unit: "$ לדקה" },
  { key: "stream_per_minute", label: "העברת הקול לשרת", unit: "$ לדקה" },
  { key: "stt_per_minute", label: "זיהוי דיבור", unit: "$ לדקה" },
  { key: "llm_input_per_million", label: "מודל: טקסט נכנס", unit: "$ למיליון טוקנים" },
  { key: "llm_cached_per_million", label: "מודל: טקסט נכנס שחוזר (מטמון)", unit: "$ למיליון טוקנים" },
  { key: "llm_output_per_million", label: "מודל: טקסט יוצא", unit: "$ למיליון טוקנים" },
  { key: "second_hearing_each", label: "שמיעה שנייה", unit: "$ לפעם" },
  { key: "tts_per_thousand_chars", label: "קול חי", unit: "$ לאלף תווים" },
  { key: "monthly_fixed", label: "תשלומים חודשיים קבועים", unit: "$ לחודש" },
  { key: "shekels_per_dollar", label: "שער הדולר", unit: "₪ לדולר" },
];

const LIMITS = [
  { value: 20, label: "20 אחרונות" },
  { value: 50, label: "50" },
  { value: 200, label: "200" },
];

export function Costs() {
  const [limit, setLimit] = useState(50);
  const costs = useApi<Costs>(`/api/costs?limit=${limit}`, 60_000);
  const c = costs.data;
  const rate = c?.rates.shekels_per_dollar ?? 3.7;
  const ils = (usd: number | null | undefined, digits = 2) => (usd == null ? "—" : `₪${(usd * rate).toFixed(digits)}`);
  const usd = (x: number | null | undefined) => (x == null ? "—" : `$${x < 0.1 ? x.toFixed(4) : x.toFixed(2)}`);

  return (
    <>
      <PageHeader
        title="עלויות"
        subtitle="כמה עלתה כל שיחה, מכל רכיב, והממוצע של השיחות האחרונות."
        action={<Segmented label="כמה שיחות" value={limit} options={LIMITS} onChange={setLimit} />}
      />
      {costs.error && <Problem>{costs.error}</Problem>}
      {!c && !costs.error && <Skeleton className="h-96 rounded-2xl" />}
      {c && (
        <div className="grid gap-6">
          <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
            <Tile label="ממוצע לשיחה" value={ils(c.summary.average.total, 3)} sub={`${usd(c.summary.average.total)} · ${c.summary.calls} שיחות`} />
            <Tile label="לדקת שיחה" value={ils(c.summary.per_minute, 3)} sub={usd(c.summary.per_minute)} />
            <Tile label={`סה״כ ${c.summary.calls} השיחות`} value={ils(c.summary.total)} sub={`${Math.round(c.summary.minutes)} דקות · ${usd(c.summary.total)}`} />
            <Tile
              label="קבוע חודשי לשיחה"
              value={ils(c.summary.fixed_per_call, 3)}
              sub={`${usd(c.rates.monthly_fixed)} לחודש על ${c.summary.calls_last_30_days} שיחות ב-30 יום`}
            />
          </div>

          <Card title="על מה הולך הכסף" subtitle="הממוצע לשיחה, לפי רכיב">
            <Breakdown average={c.summary.average} ils={ils} />
          </Card>

          <Card title="שיחה שיחה" flush>
            {c.calls.length === 0 ? (
              <Empty>עוד אין שיחות</Empty>
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full min-w-[820px] text-sm">
                  <thead>
                    <tr className="border-b border-slate-100 bg-slate-50 text-xs text-slate-500 dark:border-white/[0.06] dark:bg-white/[0.03] dark:text-slate-400">
                      <th className="px-5 py-2.5 text-start font-medium">מתי</th>
                      <th className="px-3 py-2.5 text-start font-medium">מספר</th>
                      <th className="px-3 py-2.5 text-start font-medium">משך</th>
                      {PARTS.map((p) => (
                        <th key={p.key} className="px-3 py-2.5 text-start font-medium">
                          {p.label}
                        </th>
                      ))}
                      <th className="px-5 py-2.5 text-start font-medium">סה״כ</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-slate-100 dark:divide-white/[0.06]">
                    {c.calls.map((r) => (
                      <tr key={r.id} className="hover:bg-slate-50 dark:hover:bg-white/[0.03]">
                        <td className="whitespace-nowrap px-5 py-2.5">
                          <Link to={`/calls/${r.id}`} className="inline-flex items-center gap-1 text-brand-700 hover:underline dark:text-brand-300">
                            <Ago iso={r.started_at} />
                            <ArrowLeft className="size-3" aria-hidden />
                          </Link>
                        </td>
                        <td className="ltr num whitespace-nowrap px-3 py-2.5 text-start">{phone(r.from)}</td>
                        <td className="num px-3 py-2.5">{duration(r.seconds)}</td>
                        {PARTS.map((p) => (
                          <td key={p.key} className="num px-3 py-2.5 text-slate-600 dark:text-slate-300" title={p.key === "tts" && r.cost.tts_chars != null ? `${r.cost.tts_chars} תווים` : p.key === "second_hearing" ? `${r.cost.second_hearings} פעמים` : undefined}>
                            {r.cost[p.key] == null ? <span title="לא נמדד בשיחות מלפני הדף הזה">—</span> : ils(r.cost[p.key], 3)}
                          </td>
                        ))}
                        <td className="num px-5 py-2.5 font-semibold">{ils(r.cost.total, 3)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </Card>
          <p className="-mt-3 text-xs text-slate-500 dark:text-slate-400">
            ״—״ בקול חי: שיחה מלפני שהתחלנו למדוד אותו, והסכום שלה בלעדיו. הוראות מוקלטות מראש לא עולות בכל שיחה, רק בהקלטה. לא כלול: העברה למוקדן (שיחה יוצאת) ושאלות לבוט המחירון.
          </p>

          <RatesCard rates={c.rates} defaults={c.defaults} onSaved={costs.reload} />
        </div>
      )}
    </>
  );
}

function Tile({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className={cx(SURFACE, "p-4 sm:p-5")}>
      <div className="text-[13px] font-medium text-slate-600 dark:text-slate-300">{label}</div>
      <div className="num mt-2 text-2xl font-extrabold tracking-tight sm:text-3xl">{value}</div>
      {sub && <div className="num mt-1 text-xs text-slate-500 dark:text-slate-400">{sub}</div>}
    </div>
  );
}

function Breakdown({ average, ils }: { average: Record<Part | "total", number>; ils: (usd: number, digits?: number) => string }) {
  const total = PARTS.reduce((a, p) => a + (average[p.key] ?? 0), 0) || 1;
  return (
    <div className="grid gap-4">
      <div className="flex h-4 overflow-hidden rounded-full bg-slate-100 dark:bg-white/[0.06]" role="img" aria-label="חלוקת העלות">
        {PARTS.map((p) => (
          <span key={p.key} style={{ width: `${((average[p.key] ?? 0) / total) * 100}%`, backgroundColor: p.color }} />
        ))}
      </div>
      <ul className="grid gap-2.5 sm:grid-cols-2">
        {PARTS.map((p) => (
          <li key={p.key} className="flex items-start gap-2.5 text-sm">
            <span className="mt-1.5 size-2.5 shrink-0 rounded-full" style={{ backgroundColor: p.color }} aria-hidden />
            <div className="min-w-0 flex-1">
              <div className="flex items-baseline justify-between gap-2">
                <span className="font-medium">{p.label}</span>
                <span className="num">
                  {ils(average[p.key] ?? 0, 3)} <span className="text-xs text-slate-500">({Math.round(((average[p.key] ?? 0) / total) * 100)}%)</span>
                </span>
              </div>
              <div className="text-xs text-slate-500 dark:text-slate-400">{p.what}</div>
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}

function RatesCard({ rates, defaults, onSaved }: { rates: Rates; defaults: Rates; onSaved: () => Promise<void> }) {
  const toast = useToast();
  const [form, setForm] = useState<Record<string, string>>({});
  const [problems, setProblems] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  useEffect(() => setForm(Object.fromEntries(Object.entries(rates).map(([k, v]) => [k, String(v)]))), [rates]);
  const dirty = RATE_FIELDS.some((f) => form[f.key] !== String(rates[f.key]));

  const save = async (next: Record<string, string>) => {
    setSaving(true);
    setProblems([]);
    try {
      const body = Object.fromEntries(RATE_FIELDS.map((f) => [f.key, Number(next[f.key])]));
      const res = await fetch("/api/costs/rates", {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      if (res.status === 401) {
        window.dispatchEvent(new Event("callora:unauthorized"));
        throw new Unauthorized("unauthorized");
      }
      if (res.ok) {
        toast("המחירים נשמרו. כל השיחות חושבו מחדש.", "good");
        await onSaved();
      } else {
        const e = (await res.json().catch(() => ({}))) as { problems?: string[] };
        setProblems(e.problems ?? [`לא נשמר (${res.status})`]);
      }
    } catch (e) {
      if (!(e instanceof Unauthorized)) toast("אין חיבור לשרת", "bad");
    } finally {
      setSaving(false);
    }
  };

  return (
    <Card title="המחירים שלפיהם מחושב" subtitle="ברירות המחדל לפי דפי המחירים של הספקים באוקטובר 2026. אם החשבונית שלך שונה, תקן כאן וכל השיחות יחושבו מחדש.">
      <details>
        <summary className="cursor-pointer text-sm font-medium text-brand-700 dark:text-brand-300">הצג ותקן מחירים</summary>
        <div className="mt-4 grid gap-4 sm:grid-cols-2">
          {RATE_FIELDS.map((f) => (
            <label key={f.key} className="grid gap-1.5 text-sm">
              <span className="font-medium">{f.label}</span>
              <div className="relative">
                <Input type="number" step="any" min={0} value={form[f.key] ?? ""} onChange={(e) => setForm({ ...form, [f.key]: e.target.value })} className="num pe-36" />
                <span className="pointer-events-none absolute inset-y-0 end-3 flex items-center text-xs text-slate-500">{f.unit}</span>
              </div>
              {String(defaults[f.key]) !== form[f.key] && <span className="text-xs text-slate-500">ברירת מחדל: {defaults[f.key]}</span>}
            </label>
          ))}
        </div>
        {problems.length > 0 && (
          <div className="mt-4">
            <Problem>{problems.join(" · ")}</Problem>
          </div>
        )}
        <div className="mt-5 flex flex-wrap gap-3">
          <Button variant="primary" onClick={() => void save(form)} disabled={!dirty || saving}>
            {saving ? "שומר…" : "שמירה"}
          </Button>
          <Button variant="ghost" onClick={() => void save(Object.fromEntries(Object.entries(defaults).map(([k, v]) => [k, String(v)])))} disabled={saving}>
            חזרה לברירות המחדל
          </Button>
        </div>
      </details>
    </Card>
  );
}
