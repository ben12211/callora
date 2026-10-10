import { useMemo, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { ArrowLeft, CheckCircle2, CircleDollarSign, Headset, Hourglass, PhoneCall, PhoneOff, Timer, TriangleAlert, UserCheck } from "lucide-react";
import { Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { type CallRow, type Day, type Order, type Session, type SettingsData, type Stats, useApi } from "../api";
import { change, dayLabel, dollars, duration, greeting, lastDays, longDate, pct, phone } from "../format";
import { Ago, Card, Delta, Donut, Empty, PageHeader, Problem, Ring, Segmented, Skeleton, SURFACE, Sparkline, cx } from "../ui";
import { CallAvatar, OutcomeBadge, Route, detail } from "../parts";
import { type WaOverview, whatsappTrouble } from "./WhatsApp";

const PERIODS = [
  { value: 1, label: "היום" },
  { value: 7, label: "7 ימים" },
  { value: 30, label: "30 ימים" },
];

const C = { done: "#10b981", handed: "#f59e0b", other: "#94a3b8", brand: "#6b66f2" };

function savedPeriod(): number {
  try {
    const v = Number(localStorage.getItem("callora_days"));
    return [1, 7, 30].includes(v) ? v : 7;
  } catch {
    return 7;
  }
}

type Alert = { key: string; tone: "warn" | "bad" | "info"; to: string; text: ReactNode };

export function Overview({ session }: { session: Session }) {
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
  const span = Math.max(days, 14);
  const daily = useApi<Day[]>(`/api/stats/daily?days=${span * 2}`, 60_000);
  const recent = useApi<CallRow[]>("/api/calls?limit=7", 30_000);
  const orders = useApi<Order[]>("/api/orders", 30_000);
  const whatsapp = useApi<WaOverview>("/api/whatsapp", 60_000);
  const settings = useApi<SettingsData>("/api/settings");
  const waTrouble = whatsappTrouble(whatsapp.data);

  const s = stats.data;
  const business = session.businesses[0]?.name;

  // The days as a series, and the period before this one, for the arrows.
  const series = useMemo(() => {
    const byDay = new Map((daily.data ?? []).map((r) => [r.day, r]));
    const all = lastDays(span * 2).map((day) => {
      const r = byDay.get(day);
      return { day, calls: r?.calls ?? 0, done: r?.done ?? 0, handed: r?.handed_off ?? 0 };
    });
    const sum = (rows: typeof all, k: "calls" | "done" | "handed") => rows.reduce((a, r) => a + r[k], 0);
    const now = all.slice(-days);
    const before = all.slice(-days * 2, -days);
    return {
      chart: all.slice(-span),
      spark: all.slice(-Math.max(days, 7)),
      calls: change(sum(now, "calls"), sum(before, "calls")),
      done: change(sum(now, "done"), sum(before, "done")),
      handed: change(sum(now, "handed"), sum(before, "handed")),
    };
  }, [daily.data, span, days]);

  const alerts: Alert[] = [];
  if (s && s.to_verify > 0) {
    alerts.push({ key: "verify", tone: "warn", to: "/orders?verify=1", text: s.to_verify === 1 ? "הזמנה אחת שלא ידוע אם נקלטה. כדאי לבדוק." : `${s.to_verify} הזמנות שלא ידוע אם נקלטו. כדאי לבדוק.` });
  }
  if (waTrouble) alerts.push({ key: "wa", tone: "bad", to: "/whatsapp?app=whatsapp", text: `${waTrouble}. ההזמנות נשמרות בתור עד שזה יסתדר.` });
  const desk = settings.data?.businesses[0]?.desk;
  if (desk && desk.numbers.length === 0) {
    alerts.push({ key: "desk", tone: "warn", to: "/settings", text: "לא הוגדר מספר למוקד. מתקשר שמבקש נציג ישמע שאין מוקדן פנוי." });
  }
  if (s && s.reviewed_bad > 0) {
    alerts.push({ key: "bad", tone: "info", to: "/calls?filter=bad", text: `${s.reviewed_bad} שיחות סומנו "לא תקין". אפשר להפוך אותן למקרי בדיקה.` });
  }

  const live = (recent.data ?? []).find((c) => c.outcome === null && Date.now() - new Date(c.started_at).getTime() < 15 * 60_000);

  return (
    <>
      <PageHeader
        title={greeting()}
        subtitle={
          <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
            <span>{longDate()}</span>
            {business && <span className="text-slate-400">·</span>}
            {business && <span>{business}</span>}
            {live && (
              <Link to={`/calls/${live.id}`} className="inline-flex items-center gap-1.5 rounded-full bg-brand-50 px-2.5 py-0.5 text-xs font-medium text-brand-700 ring-1 ring-inset ring-brand-200 dark:bg-brand-400/15 dark:text-brand-100 dark:ring-brand-400/25">
                <span className="relative flex size-2">
                  <span className="absolute inline-flex size-full animate-ping-slow rounded-full bg-brand-400 opacity-70" />
                  <span className="relative inline-flex size-2 rounded-full bg-brand-500" />
                </span>
                שיחה פעילה עכשיו
              </Link>
            )}
          </span>
        }
        action={<Segmented label="תקופה" value={days} options={PERIODS} onChange={choose} />}
      />

      {stats.error && <Problem>{stats.error}</Problem>}

      {alerts.length > 0 && (
        <div className="mb-6 grid gap-2.5 lg:grid-cols-2">
          {alerts.map((a) => (
            <AlertRow key={a.key} alert={a} />
          ))}
        </div>
      )}

      {/* The four numbers that say how the line is doing. */}
      <div className="grid grid-cols-2 gap-3 sm:gap-4 xl:grid-cols-4">
        {!s ? (
          Array.from({ length: 4 }, (_, i) => <Skeleton key={i} className="h-36 rounded-2xl sm:h-40" />)
        ) : (
          <>
            <Kpi
              label="שיחות"
              icon={<PhoneCall className="size-4" aria-hidden />}
              value={s.calls}
              delta={series.calls}
              deltaLabel={days === 1 ? "מאתמול" : `מ-${days} הימים שלפני`}
              spark={series.spark.map((r) => r.calls)}
              color={C.brand}
            />
            <Kpi
              label="הוזמנה מונית, בלי מוקדן"
              icon={<CheckCircle2 className="size-4" aria-hidden />}
              value={pct(s.done_without_a_person_share)}
              delta={series.done}
              deltaLabel={days === 1 ? "מאתמול" : `מ-${days} הימים שלפני`}
              sub={`${s.done_without_a_person} מתוך ${s.calls} שיחות`}
              ring={s.done_without_a_person_share ?? 0}
              color={C.done}
            />
            <Kpi
              label="הועברו למוקדן"
              icon={<Headset className="size-4" aria-hidden />}
              value={s.handed_off}
              sub={`${pct(s.handed_off_share)} מהשיחות`}
              delta={series.handed}
              goodWhen="none"
              deltaLabel={days === 1 ? "מאתמול" : `מ-${days} הימים שלפני`}
              spark={series.spark.map((r) => r.handed)}
              color={C.handed}
            />
            <Kpi
              label="זמן עד הזמנה"
              icon={<Timer className="size-4" aria-hidden />}
              value={duration(s.booking_seconds_median)}
              sub={s.booking_turns_median == null ? "חציון" : `חציון · ${s.booking_turns_median} תורות של המתקשר`}
              color={C.brand}
            />
          </>
        )}
      </div>

      {s && (
        <div className="mt-4 grid grid-cols-2 gap-3 lg:grid-cols-4">
          <Mini icon={<PhoneOff className="size-4" aria-hidden />} label="ניתקו בלי תוצאה" value={s.nothing_done} sub="לפני שהושלם משהו" tone={s.nothing_done > 0 ? "warn" : undefined} to="/calls?filter=nothing" />
          <Mini icon={<Hourglass className="size-4" aria-hidden />} label="משך שיחה ממוצע" value={duration(s.avg_duration_seconds)} />
          <Mini icon={<CircleDollarSign className="size-4" aria-hidden />} label="עלות לשיחה" value={dollars(s.cost_per_call)} sub={s.cost == null ? "חסרים מחירים" : `סה״כ ${dollars(s.cost)}`} />
          <Mini
            icon={<UserCheck className="size-4" aria-hidden />}
            label="בדיקה ידנית"
            value={s.reviewed_good + s.reviewed_bad || "—"}
            sub={s.reviewed_good + s.reviewed_bad ? `${s.reviewed_good} תקין · ${s.reviewed_bad} לא תקין` : "עוד לא סומנו שיחות"}
            tone={s.reviewed_bad > 0 ? "bad" : undefined}
            to="/calls"
          />
        </div>
      )}

      <div className="mt-6 grid gap-6 xl:grid-cols-3">
        <Card title="שיחות לפי יום" subtitle={`${span} הימים האחרונים`} className="xl:col-span-2">
          <DailyChart rows={series.chart} loading={daily.loading && !daily.data} error={daily.error} />
        </Card>
        <Card title="איך נגמרו השיחות" subtitle={days === 1 ? "היום" : `${days} הימים האחרונים`}>
          {s ? <Outcomes s={s} /> : <Skeleton className="h-52" />}
        </Card>
      </div>

      <div className="mt-6 grid gap-6 xl:grid-cols-5">
        <Card
          title="שיחות אחרונות"
          className="xl:col-span-3"
          action={
            <Link to="/calls" className="inline-flex items-center gap-1 text-sm font-medium text-brand-700 hover:underline dark:text-brand-300">
              כל השיחות
              <ArrowLeft className="size-3.5" aria-hidden />
            </Link>
          }
        >
          {recent.error ? (
            <Problem>{recent.error}</Problem>
          ) : !recent.data ? (
            <div className="grid gap-3">{Array.from({ length: 5 }, (_, i) => <Skeleton key={i} className="h-12" />)}</div>
          ) : recent.data.length === 0 ? (
            <Empty icon={<PhoneCall className="size-5" aria-hidden />}>אין שיחות עדיין. הן יופיעו כאן ברגע שמישהו יתקשר.</Empty>
          ) : (
            <ul className="-mx-2 -my-1 divide-y divide-slate-100 dark:divide-white/[0.06]">
              {recent.data.map((c) => (
                <li key={c.id}>
                  <Link to={`/calls/${c.id}`} className="group flex items-center gap-3.5 rounded-xl px-2 py-3 transition-colors hover:bg-slate-50 dark:hover:bg-white/[0.04]">
                    <CallAvatar outcome={c.outcome} orders={c.orders} />
                    <div className="min-w-0 flex-1">
                      <div className="ltr num text-start text-sm font-semibold">{phone(c.from)}</div>
                      <Ago iso={c.started_at} className="text-xs text-slate-500 dark:text-slate-400" />
                    </div>
                    <span className="num hidden text-xs text-slate-500 sm:inline dark:text-slate-400">{duration(c.duration_seconds)}</span>
                    <OutcomeBadge outcome={c.outcome} orders={c.orders} />
                    <ArrowLeft className="size-4 text-slate-300 transition-transform group-hover:-translate-x-0.5 group-hover:text-slate-500 dark:text-slate-600" aria-hidden />
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </Card>

        <Card
          title="הזמנות אחרונות"
          className="xl:col-span-2"
          action={
            <Link to="/orders" className="inline-flex items-center gap-1 text-sm font-medium text-brand-700 hover:underline dark:text-brand-300">
              כל ההזמנות
              <ArrowLeft className="size-3.5" aria-hidden />
            </Link>
          }
        >
          {orders.error ? (
            <Problem>{orders.error}</Problem>
          ) : !orders.data ? (
            <Skeleton className="h-52" />
          ) : orders.data.length === 0 ? (
            <Empty>אין הזמנות עדיין</Empty>
          ) : (
            <ul className="-my-1 divide-y divide-slate-100 dark:divide-white/[0.06]">
              {orders.data.slice(0, 4).map((o, i) => (
                <li key={`${o.call_id}-${i}`} className="py-3.5">
                  <Link to={`/calls/${o.call_id}`} className="block rounded-lg transition-opacity hover:opacity-80">
                    <div className="mb-2 flex items-center justify-between gap-2">
                      <Ago iso={o.at} className="text-xs text-slate-500 dark:text-slate-400" />
                      <span className="num text-xs text-slate-500 dark:text-slate-400">
                        {detail(o, "passengers") ? `${detail(o, "passengers")} נוסעים` : ""}
                      </span>
                    </div>
                    <Route compact from={detail(o, "pickup") ?? "—"} to={detail(o, "destination") ?? "—"} />
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

function AlertRow({ alert: a }: { alert: Alert }) {
  const tones = {
    warn: "bg-amber-50 text-amber-900 ring-amber-200 hover:bg-amber-100/70 dark:bg-amber-400/10 dark:text-amber-100 dark:ring-amber-400/20 dark:hover:bg-amber-400/15",
    bad: "bg-rose-50 text-rose-900 ring-rose-200 hover:bg-rose-100/70 dark:bg-rose-400/10 dark:text-rose-100 dark:ring-rose-400/20 dark:hover:bg-rose-400/15",
    info: "bg-brand-50 text-brand-900 ring-brand-200 hover:bg-brand-100/60 dark:bg-brand-400/10 dark:text-brand-100 dark:ring-brand-400/20 dark:hover:bg-brand-400/15",
  };
  return (
    <Link to={a.to} className={cx("flex items-center gap-3 rounded-xl px-4 py-3 text-sm ring-1 ring-inset transition-colors", tones[a.tone])}>
      <TriangleAlert className="size-4 shrink-0" aria-hidden />
      <span className="flex-1 leading-snug">{a.text}</span>
      <ArrowLeft className="size-4 shrink-0 opacity-60" aria-hidden />
    </Link>
  );
}

function Kpi({
  label,
  icon,
  value,
  sub,
  delta,
  deltaLabel,
  goodWhen = "up",
  spark,
  ring,
  color,
}: {
  label: string;
  icon: ReactNode;
  value: ReactNode;
  sub?: ReactNode;
  delta?: number | null;
  deltaLabel?: string;
  goodWhen?: "up" | "down" | "none";
  spark?: number[];
  ring?: number;
  color: string;
}) {
  return (
    <div className={cx(SURFACE, "flex animate-rise flex-col overflow-hidden p-4 sm:p-5")}>
      <div className="flex items-start justify-between gap-2">
        <span className="text-[13px] font-medium leading-tight text-slate-600 dark:text-slate-300">{label}</span>
        <span className="flex size-8 shrink-0 items-center justify-center rounded-lg" style={{ backgroundColor: `${color}1f`, color }}>
          {icon}
        </span>
      </div>
      <div className="mt-3 flex items-end justify-between gap-3">
        <div className="min-w-0">
          <div className="num text-[2rem] font-extrabold leading-none tracking-tight text-slate-900 sm:text-[2.4rem] dark:text-white">{value}</div>
          <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs text-slate-500 dark:text-slate-400">
            {delta !== undefined && <Delta value={delta} goodWhen={goodWhen} />}
            {delta !== undefined && deltaLabel && delta != null && <span>{deltaLabel}</span>}
            {sub && <span>{sub}</span>}
          </div>
        </div>
        {ring !== undefined && (
          <span className="hidden sm:block">
            <Ring share={ring} color={color} size={68} stroke={8} />
          </span>
        )}
      </div>
      {spark && spark.length > 1 && (
        <div className="-mx-4 -mb-4 mt-3 sm:-mx-5 sm:-mb-5 sm:mt-4">
          <Sparkline data={spark} color={color} className="h-9 sm:h-12" />
        </div>
      )}
    </div>
  );
}

function Mini({ icon, label, value, sub, tone, to }: { icon: ReactNode; label: string; value: ReactNode; sub?: ReactNode; tone?: "warn" | "bad"; to?: string }) {
  const body = (
    <>
      <span
        className={cx(
          "flex size-9 shrink-0 items-center justify-center rounded-xl",
          tone === "warn" ? "bg-amber-100 text-amber-700 dark:bg-amber-400/15 dark:text-amber-300" : tone === "bad" ? "bg-rose-100 text-rose-700 dark:bg-rose-400/15 dark:text-rose-300" : "bg-slate-100 text-slate-500 dark:bg-white/[0.07] dark:text-slate-400",
        )}
      >
        {icon}
      </span>
      <div className="min-w-0">
        <div className="truncate text-xs text-slate-500 dark:text-slate-400">{label}</div>
        <div className="num text-lg font-bold leading-tight">{value}</div>
        {sub && <div className="truncate text-[11px] text-slate-500 dark:text-slate-400">{sub}</div>}
      </div>
    </>
  );
  const cls = cx(SURFACE, "flex animate-rise items-center gap-3 p-3.5");
  return to ? (
    <Link to={to} className={cx(cls, "transition-shadow hover:shadow-pop")}>
      {body}
    </Link>
  ) : (
    <div className={cls}>{body}</div>
  );
}

function Outcomes({ s }: { s: Stats }) {
  const other = Math.max(s.calls - s.done_without_a_person - s.handed_off - s.nothing_done, 0);
  const parts = [
    { label: "הוזמנה מונית", value: s.done_without_a_person, color: C.done },
    { label: "הועברו למוקדן", value: s.handed_off, color: C.handed },
    { label: "ניתקו בלי תוצאה", value: s.nothing_done, color: C.other },
    { label: "אחר", value: other, color: "#cbd5e1" },
  ].filter((p) => p.value > 0);
  if (s.calls === 0) return <Empty>אין שיחות בתקופה הזאת</Empty>;
  return (
    <div className="flex flex-col items-center gap-5">
      <Donut parts={parts} size={156} stroke={20}>
        <div className="num text-3xl font-extrabold leading-none">{s.calls}</div>
        <div className="mt-1 text-xs text-slate-500 dark:text-slate-400">שיחות</div>
      </Donut>
      <ul className="w-full space-y-2.5">
        {parts.map((p) => (
          <li key={p.label} className="flex items-center gap-2.5 text-sm">
            <span className="size-2.5 rounded-full" style={{ backgroundColor: p.color }} aria-hidden />
            <span className="flex-1 text-slate-700 dark:text-slate-200">{p.label}</span>
            <span className="num font-semibold">{p.value}</span>
            <span className="num w-10 text-end text-xs text-slate-500 dark:text-slate-400">{Math.round((p.value / s.calls) * 100)}%</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

type ChartRow = { day: string; calls: number; done: number; handed: number };

function DailyChart({ rows, loading, error }: { rows: ChartRow[]; loading: boolean; error: string | null }) {
  const data = useMemo(
    () => rows.map((r) => ({ day: dayLabel(r.day), done: r.done, handed: r.handed, other: Math.max(r.calls - r.done - r.handed, 0), calls: r.calls })),
    [rows],
  );
  if (error) return <Problem>{error}</Problem>;
  if (loading) return <Skeleton className="h-72" />;
  if (data.every((d) => d.calls === 0)) return <Empty>עוד אין שיחות בתקופה הזאת</Empty>;
  return (
    <div>
      <div className="h-72 text-slate-500 dark:text-slate-400" dir="ltr">
        <ResponsiveContainer width="100%" height="100%">
          <BarChart data={data} margin={{ top: 6, right: 4, left: -22, bottom: 0 }} barCategoryGap="22%">
            <CartesianGrid strokeDasharray="3 4" vertical={false} stroke="currentColor" strokeOpacity={0.18} />
            <XAxis dataKey="day" reversed tickLine={false} axisLine={false} tick={{ fontSize: 11, fill: "currentColor" }} interval="preserveStartEnd" tickMargin={8} />
            <YAxis orientation="right" allowDecimals={false} tickLine={false} axisLine={false} tick={{ fontSize: 11, fill: "currentColor" }} />
            <Tooltip cursor={{ fill: "currentColor", fillOpacity: 0.07 }} content={<ChartTip />} />
            <Bar dataKey="done" stackId="a" fill={C.done} isAnimationActive={false} />
            <Bar dataKey="handed" stackId="a" fill={C.handed} isAnimationActive={false} />
            <Bar dataKey="other" stackId="a" fill="#cbd5e1" radius={[5, 5, 0, 0]} isAnimationActive={false} />
          </BarChart>
        </ResponsiveContainer>
      </div>
      <div className="mt-3 flex flex-wrap gap-x-5 gap-y-1 text-xs text-slate-600 dark:text-slate-300">
        {[
          ["הוזמנה מונית", C.done],
          ["הועברו למוקדן", C.handed],
          ["אחר", "#cbd5e1"],
        ].map(([l, c]) => (
          <span key={l} className="inline-flex items-center gap-1.5">
            <span className="size-2.5 rounded-sm" style={{ backgroundColor: c }} aria-hidden />
            {l}
          </span>
        ))}
      </div>
    </div>
  );
}

function ChartTip({ active, payload, label }: { active?: boolean; payload?: { payload: { calls: number; done: number; handed: number; other: number } }[]; label?: string }) {
  if (!active || !payload?.length) return null;
  const p = payload[0].payload;
  const row = (name: string, v: number, color: string) => (
    <div className="flex items-center gap-2">
      <span className="size-2 rounded-sm" style={{ backgroundColor: color }} />
      <span className="flex-1">{name}</span>
      <span className="num font-semibold">{v}</span>
    </div>
  );
  return (
    <div dir="rtl" className="min-w-40 rounded-xl bg-slate-900 px-3.5 py-2.5 text-xs text-white shadow-pop ring-1 ring-white/10">
      <div className="mb-1.5 flex items-center justify-between gap-4 font-semibold">
        <span>{label}</span>
        <span className="num text-slate-300">{p.calls} שיחות</span>
      </div>
      <div className="space-y-1">
        {row("הוזמנה מונית", p.done, C.done)}
        {row("הועברו למוקדן", p.handed, C.handed)}
        {row("אחר", p.other, "#cbd5e1")}
      </div>
    </div>
  );
}
