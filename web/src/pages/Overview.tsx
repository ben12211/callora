import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { ArrowLeft, TriangleAlert } from "lucide-react";
import { Bar, BarChart, CartesianGrid, Legend, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { type CallRow, type Day, type Stats, useApi } from "../api";
import { dayLabel, dollars, duration, lastDays, pct, phone } from "../format";
import { Card, Empty, Loading, PageHeader, Problem, Segmented, Stat, When } from "../ui";
import { OutcomeBadge } from "./Calls";
import { type WaOverview, whatsappTrouble } from "./WhatsApp";

const PERIODS = [
  { value: 1, label: "היום" },
  { value: 7, label: "7 ימים" },
  { value: 30, label: "30 ימים" },
];

function savedPeriod(): number {
  try {
    const v = Number(localStorage.getItem("callora_days"));
    return [1, 7, 30].includes(v) ? v : 7;
  } catch {
    return 7;
  }
}

export function Overview() {
  const [days, setDays] = useState(savedPeriod);
  const choose = (d: number) => {
    setDays(d);
    try {
      localStorage.setItem("callora_days", String(d));
    } catch {
      /* private mode */
    }
  };
  const stats = useApi<Stats>(`/api/stats?days=${days}`, 30_000);
  const chartDays = Math.max(days, 14);
  const daily = useApi<Day[]>(`/api/stats/daily?days=${chartDays}`, 60_000);
  const recent = useApi<CallRow[]>("/api/calls?limit=8", 30_000);
  const whatsapp = useApi<WaOverview>("/api/whatsapp", 60_000);
  const waTrouble = whatsappTrouble(whatsapp.data);

  const s = stats.data;
  return (
    <>
      <PageHeader title="סקירה" subtitle="מה קורה בקו, במבט אחד" action={<Segmented label="תקופה" value={days} options={PERIODS} onChange={choose} />} />

      {stats.error && <Problem>{stats.error}</Problem>}
      {s && s.to_verify > 0 && (
        <Link
          to="/orders?verify=1"
          className="mb-6 flex items-center gap-3 rounded-xl border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-900 hover:bg-amber-100 dark:border-amber-900 dark:bg-amber-950/50 dark:text-amber-200"
        >
          <TriangleAlert className="size-4 shrink-0" aria-hidden />
          <span className="flex-1">
            {s.to_verify === 1 ? "הזמנה אחת שלא ידוע אם נקלטה" : `${s.to_verify} הזמנות שלא ידוע אם נקלטו`} במערכת ההזמנות. כדאי לבדוק.
          </span>
          <ArrowLeft className="size-4" aria-hidden />
        </Link>
      )}

      {waTrouble && (
        <Link
          to="/whatsapp"
          className="mb-6 flex items-center gap-3 rounded-xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-900 hover:bg-rose-100 dark:border-rose-900 dark:bg-rose-950/50 dark:text-rose-200"
        >
          <TriangleAlert className="size-4 shrink-0" aria-hidden />
          <span className="flex-1">{waTrouble}. ההזמנות נשמרות בתור עד שזה יסתדר.</span>
          <ArrowLeft className="size-4" aria-hidden />
        </Link>
      )}

      {!s && stats.loading ? (
        <Loading />
      ) : s ? (
        <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
          <Stat label="שיחות" value={s.calls} />
          <Stat label="בוצע בלי מוקדן" value={pct(s.done_without_a_person_share)} sub={`${s.done_without_a_person} שיחות`} tone="good" />
          <Stat label="הועבר למוקדן" value={pct(s.handed_off_share)} sub={`${s.handed_off} שיחות`} />
          <Stat label="בלי תוצאה" value={s.nothing_done} sub="ניתקו לפני שהושלם משהו" tone={s.nothing_done ? "warn" : undefined} />
          <Stat
            label="זמן עד הזמנה"
            value={duration(s.booking_seconds_median)}
            sub={s.booking_turns_median == null ? "חציון" : `חציון · ${s.booking_turns_median} תורות של המתקשר`}
          />
          <Stat label="משך שיחה ממוצע" value={duration(s.avg_duration_seconds)} />
          <Stat label="עלות לשיחה" value={dollars(s.cost_per_call)} sub={s.cost == null ? "חסרים מחירים (AGENT_PRICES)" : `סה״כ ${dollars(s.cost)}`} />
          <Stat label="סומנו לא תקין" value={s.reviewed_bad} sub={`${s.reviewed_good} סומנו תקין`} tone={s.reviewed_bad ? "bad" : undefined} />
        </div>
      ) : null}

      <div className="mt-6 grid gap-6 xl:grid-cols-5">
        <Card title={`שיחות לפי יום · ${chartDays} ימים אחרונים`} className="xl:col-span-3">
          <DailyChart days={chartDays} rows={daily.data} loading={daily.loading} error={daily.error} />
        </Card>
        <Card
          title="שיחות אחרונות"
          className="xl:col-span-2"
          action={
            <Link to="/calls" className="text-sm font-medium text-brand-700 hover:underline dark:text-brand-300">
              לכל השיחות
            </Link>
          }
        >
          {recent.error ? (
            <Problem>{recent.error}</Problem>
          ) : !recent.data ? (
            <Loading />
          ) : recent.data.length === 0 ? (
            <Empty>אין שיחות עדיין</Empty>
          ) : (
            <ul className="-my-2 divide-y divide-slate-100 dark:divide-slate-800">
              {recent.data.map((c) => (
                <li key={c.id}>
                  <Link to={`/calls/${c.id}`} className="-mx-2 flex items-center gap-3 rounded-lg px-2 py-2.5 hover:bg-slate-50 dark:hover:bg-slate-800/60">
                    <div className="min-w-0 flex-1">
                      <div className="text-sm font-medium">
                        <span className="ltr tabular-nums">{phone(c.from)}</span>
                      </div>
                      <When iso={c.started_at} className="text-xs text-slate-500 dark:text-slate-400" />
                    </div>
                    {c.orders > 0 && <span className="text-xs text-slate-500">{c.orders} הזמנות</span>}
                    <OutcomeBadge outcome={c.outcome} />
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </Card>
      </div>
    </>
  );
}

function DailyChart({ days, rows, loading, error }: { days: number; rows: Day[] | null; loading: boolean; error: string | null }) {
  const data = useMemo(() => {
    const byDay = new Map((rows ?? []).map((r) => [r.day, r]));
    return lastDays(days).map((day) => {
      const r = byDay.get(day);
      const calls = r?.calls ?? 0;
      const done = r?.done ?? 0;
      const handed = r?.handed_off ?? 0;
      return { day: dayLabel(day), "בוצע בלי מוקדן": done, "הועבר למוקדן": handed, אחר: Math.max(calls - done - handed, 0) };
    });
  }, [rows, days]);

  if (error) return <Problem>{error}</Problem>;
  if (!rows && loading) return <Loading />;
  return (
    <div className="h-72" dir="ltr">
      <ResponsiveContainer width="100%" height="100%">
        <BarChart data={data} margin={{ top: 8, right: 8, left: -16, bottom: 0 }}>
          <CartesianGrid strokeDasharray="3 3" vertical={false} stroke="currentColor" className="text-slate-200 dark:text-slate-800" />
          <XAxis dataKey="day" reversed tickLine={false} axisLine={false} tick={{ fontSize: 11, fill: "#64748b" }} interval="preserveStartEnd" />
          <YAxis orientation="right" allowDecimals={false} tickLine={false} axisLine={false} tick={{ fontSize: 11, fill: "#64748b" }} />
          <Tooltip
            cursor={{ fill: "rgba(100,116,139,0.08)" }}
            contentStyle={{ borderRadius: 8, border: "1px solid #e2e8f0", fontSize: 12, direction: "rtl" }}
          />
          <Legend wrapperStyle={{ fontSize: 12, direction: "rtl" }} iconType="circle" iconSize={8} />
          <Bar dataKey="בוצע בלי מוקדן" stackId="a" fill="#4a72b8" />
          <Bar dataKey="הועבר למוקדן" stackId="a" fill="#d97706" />
          <Bar dataKey="אחר" stackId="a" fill="#94a3b8" radius={[4, 4, 0, 0]} />
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
}
