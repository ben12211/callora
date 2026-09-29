import { useEffect, useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { ArrowRight, Bot, Cpu, Download, Headset, ThumbsDown, ThumbsUp, TriangleAlert, User, Zap } from "lucide-react";
import { api, type CallDetail, type Turn, useApi, type Verdict } from "../api";
import { ACTIONS, clock, duration, phone, secondsOf, SLOTS, tokens } from "../format";
import { Badge, Button, Card, cx, Empty, FIELD, Problem, Skeleton, SURFACE, useToast, When } from "../ui";
import { CallAvatar, OutcomeBadge, VerdictBadge } from "../parts";

export function CallView() {
  const { id = "" } = useParams();
  const call = useApi<CallDetail>(`/api/calls/${encodeURIComponent(id)}`);
  const c = call.data;

  const back = (
    <Link to="/calls" className="mb-5 inline-flex items-center gap-1.5 rounded-lg px-1 py-1 text-sm font-medium text-slate-500 transition-colors hover:text-brand-700 dark:text-slate-400 dark:hover:text-brand-300">
      <ArrowRight className="size-4" aria-hidden />
      כל השיחות
    </Link>
  );

  const callerTurns = c?.turns.filter((t) => t.speaker === "caller") ?? [];
  const decisions = callerTurns.map((t) => t.detail?.decision_ms).filter((m): m is number => typeof m === "number");
  const avgMs = decisions.length ? decisions.reduce((a, b) => a + b, 0) / decisions.length : null;

  if (call.error) {
    return (
      <>
        {back}
        <Problem>{call.error === "הנתונים לא נטענו" ? "השיחה לא נמצאה" : call.error}</Problem>
      </>
    );
  }
  if (!c) {
    return (
      <>
        {back}
        <Skeleton className="mb-6 h-32 rounded-2xl" />
        <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_24rem]">
          <Skeleton className="h-96 rounded-2xl" />
          <Skeleton className="h-64 rounded-2xl" />
        </div>
      </>
    );
  }

  const seconds = c.ended_at ? (new Date(c.ended_at).getTime() - new Date(c.started_at).getTime()) / 1000 : null;
  const booked = c.actions.some((a) => a.ok);

  return (
    <>
      {back}

      <div className={cx(SURFACE, "mb-6 animate-rise p-5 sm:p-6")}>
        <div className="flex flex-wrap items-center gap-x-5 gap-y-4">
          <CallAvatar outcome={c.outcome} orders={booked ? 1 : 0} size="lg" />
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
              <h1 className="ltr num text-2xl font-extrabold tracking-tight">{phone(c.from)}</h1>
              <OutcomeBadge outcome={c.outcome} orders={booked ? 1 : 0} />
              <VerdictBadge verdict={c.review?.verdict ?? null} />
            </div>
            <p className="mt-1 text-sm text-slate-500 dark:text-slate-400">
              <When iso={c.started_at} />
            </p>
          </div>
        </div>
        <dl className="mt-5 grid grid-cols-2 gap-3 border-t border-slate-100 pt-5 sm:grid-cols-4 dark:border-white/[0.06]">
          <Fact label="משך השיחה" value={duration(seconds)} />
          <Fact label="תורות של המתקשר" value={callerTurns.length} />
          <Fact label="זמן החלטה ממוצע" value={avgMs == null ? "—" : secondsOf(avgMs)} tone={avgMs == null ? undefined : speed(avgMs)} />
          <Fact label="מודל" value={c.usage ? c.usage.model : "—"} sub={c.usage ? `${tokens(c.usage.input)} נכנסים · ${tokens(c.usage.output)} יוצאים` : undefined} small />
        </dl>
      </div>

      <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_24rem]">
        <Card title="השיחה" subtitle="כל משפט, ומה הסוכן החליט אחריו">
          {c.turns.length === 0 ? (
            <Empty>התמליל נמחק (לפי זמן השמירה) או שהשיחה ריקה</Empty>
          ) : (
            <ol className="flex flex-col gap-5">
              {c.turns.map((t, i) => (
                <TurnView key={i} turn={t} />
              ))}
            </ol>
          )}
        </Card>

        <div className="flex flex-col gap-6">
          <Review call={c} />
          {c.handoffs.map((h, i) => (
            <Card key={i} title={<span className="inline-flex items-center gap-2"><Headset className="size-4 text-amber-500" aria-hidden />הועבר למוקדן</span>}>
              <div className="text-xs text-slate-500 dark:text-slate-400">{h.reason}</div>
              {h.summary?.text && <p className="mt-2 text-sm leading-relaxed">{h.summary.text}</p>}
            </Card>
          ))}
          {c.actions.length > 0 && (
            <Card title="פעולות במערכת">
              <ul className="flex flex-col gap-2.5">
                {c.actions.map((a, i) => {
                  const unknown = a.result?.error?.outcome_unknown;
                  return (
                    <li key={i} className="flex items-center gap-2.5 text-sm">
                      <span className="ltr font-medium">{a.action}</span>
                      <Badge tone={a.ok ? "good" : unknown ? "warn" : "bad"} dot>
                        {a.ok ? "הצליח" : unknown ? "לא ידוע אם נקלט" : "נכשל"}
                      </Badge>
                      <span className="num ms-auto text-xs text-slate-500 dark:text-slate-400">{secondsOf(a.latency_ms)}</span>
                    </li>
                  );
                })}
              </ul>
            </Card>
          )}
          {c.utterances.length > 0 && (
            <Card title="הקלטות המתקשר" subtitle="נשמרות רק ממספרי בדיקה">
              <ul className="flex flex-col gap-3.5">
                {c.utterances.map((u) => (
                  <li key={u.id}>
                    <div className="mb-1.5 text-xs text-slate-600 dark:text-slate-300">“{u.heard}”</div>
                    <audio controls preload="none" src={`/api/utterances/${u.id}`} className="h-9 w-full" />
                  </li>
                ))}
              </ul>
            </Card>
          )}
        </div>
      </div>
    </>
  );
}

/** Green under a second, amber under two, rose beyond: how it feels on the phone. */
function speed(ms: number): "good" | "warn" | "bad" {
  return ms < 1200 ? "good" : ms < 2000 ? "warn" : "bad";
}

const SPEED_PILL = {
  good: "bg-emerald-50 text-emerald-700 ring-emerald-200 dark:bg-emerald-400/10 dark:text-emerald-300 dark:ring-emerald-400/20",
  warn: "bg-amber-50 text-amber-800 ring-amber-200 dark:bg-amber-400/10 dark:text-amber-300 dark:ring-amber-400/20",
  bad: "bg-rose-50 text-rose-700 ring-rose-200 dark:bg-rose-400/10 dark:text-rose-300 dark:ring-rose-400/20",
};

function Fact({ label, value, sub, tone, small }: { label: string; value: React.ReactNode; sub?: string; tone?: "good" | "warn" | "bad"; small?: boolean }) {
  const color = tone === "good" ? "text-emerald-600 dark:text-emerald-400" : tone === "warn" ? "text-amber-600 dark:text-amber-400" : tone === "bad" ? "text-rose-600 dark:text-rose-400" : "";
  return (
    <div className="min-w-0">
      <dt className="text-xs text-slate-500 dark:text-slate-400">{label}</dt>
      <dd className={cx("num mt-0.5 truncate font-bold", small ? "text-sm" : "text-xl", color)}>{value}</dd>
      {sub && <dd className="num truncate text-[11px] text-slate-500 dark:text-slate-400">{sub}</dd>}
    </div>
  );
}

function Pill({ children, className, icon }: { children: React.ReactNode; className?: string; icon?: React.ReactNode }) {
  return (
    <span className={cx("inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[11px] font-medium ring-1 ring-inset", className ?? "bg-slate-50 text-slate-600 ring-slate-200 dark:bg-white/[0.05] dark:text-slate-300 dark:ring-white/10")}>
      {icon}
      {children}
    </span>
  );
}

function TurnView({ turn }: { turn: Turn }) {
  const agent = turn.speaker === "agent";
  const d = turn.detail ?? {};
  const r = d.route === "agent" ? d.reply ?? {} : null;
  const fields = useMemo(() => r?.fields ?? [], [r]);
  return (
    <li className={cx("flex gap-3", agent ? "flex-row" : "flex-row-reverse")}>
      <span
        className={cx(
          "mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-full",
          agent ? "bg-gradient-to-br from-brand-400 to-brand-700 text-white shadow-sm shadow-brand-600/30" : "bg-slate-200 text-slate-600 dark:bg-white/10 dark:text-slate-300",
        )}
        aria-hidden
      >
        {agent ? <Bot className="size-4" /> : <User className="size-4" />}
      </span>
      <div className={cx("flex min-w-0 max-w-[85%] flex-col gap-1.5", agent ? "items-start" : "items-end")}>
        <div className={cx("flex items-center gap-2 text-[11px] text-slate-500 dark:text-slate-400", !agent && "flex-row-reverse")}>
          <span className="font-semibold">{agent ? "קלורה" : "מתקשר"}</span>
          <span className="num">{clock(turn.at)}</span>
        </div>
        <div
          className={cx(
            "rounded-2xl px-4 py-2.5 text-[15px] leading-relaxed",
            agent ? "rounded-ss-md bg-brand-600 text-white shadow-sm shadow-brand-600/20 dark:bg-brand-500/90" : "rounded-se-md bg-slate-100 text-slate-900 dark:bg-white/[0.08] dark:text-slate-100",
          )}
        >
          {turn.text}
        </div>

        {(r || d.transcript != null) && !agent && (
          <div className="flex flex-wrap items-center gap-1.5">
            {r ? (
              <>
                {d.decision_ms != null && (
                  <Pill className={SPEED_PILL[speed(d.decision_ms)]} icon={<Zap className="size-3" aria-hidden />}>
                    <span className="num">{secondsOf(d.decision_ms)}</span>
                  </Pill>
                )}
                {r.action && ACTIONS[r.action] && <Pill className="bg-brand-50 text-brand-700 ring-brand-200 dark:bg-brand-400/10 dark:text-brand-200 dark:ring-brand-400/20">{ACTIONS[r.action]}</Pill>}
                {r.phrase && <Pill>הקלטה · {r.phrase}</Pill>}
                {fields.map((f) => (
                  <Pill key={f.slot + f.value} className="bg-emerald-50 text-emerald-800 ring-emerald-200 dark:bg-emerald-400/10 dark:text-emerald-200 dark:ring-emerald-400/20">
                    {SLOTS[f.slot] ?? f.slot}: {f.value}
                  </Pill>
                ))}
              </>
            ) : (
              <Pill icon={<Cpu className="size-3" aria-hidden />}>נענה בחוקים, בלי הסוכן</Pill>
            )}
          </div>
        )}
        {d.second_hearing && <div className="text-[11px] text-slate-500 dark:text-slate-400">שמיעה שנייה: {d.second_hearing}</div>}
        {d.held_for_open_question && (
          <div className="inline-flex items-center gap-1 text-[11px] text-amber-700 dark:text-amber-400">
            <TriangleAlert className="size-3" aria-hidden />
            עבר לשאלה אחרת בלי תשובה על {SLOTS[d.held_for_open_question] ?? d.held_for_open_question}: נשאל שוב
          </div>
        )}
        {d.error && <div className="text-[11px] text-rose-600 dark:text-rose-400">שגיאה: {d.error}</div>}
      </div>
    </li>
  );
}

function Review({ call }: { call: CallDetail }) {
  const toast = useToast();
  const [verdict, setVerdict] = useState<Verdict | null>(call.review?.verdict ?? null);
  const [note, setNote] = useState(call.review?.note ?? "");
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setVerdict(call.review?.verdict ?? null);
    setNote(call.review?.note ?? "");
  }, [call.id, call.review]);

  const save = async (v: Verdict) => {
    setSaving(true);
    try {
      await api(`/api/calls/${call.id}/review`, { method: "PUT", body: JSON.stringify({ verdict: v, note }) });
      setVerdict(v);
      toast(v === "good" ? "סומן כתקין" : "סומן כלא תקין", "good");
    } catch {
      toast("לא נשמר", "bad");
    } finally {
      setSaving(false);
    }
  };

  const exportCase = async () => {
    try {
      const data = await api<unknown>(`/api/calls/${call.id}/eval-case`);
      const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2) + "\n"], { type: "application/json" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = `call_${call.call_sid || call.id}.json`;
      a.click();
      URL.revokeObjectURL(url);
    } catch {
      toast("הייצוא נכשל", "bad");
    }
  };

  return (
    <Card title="איך הסוכן היה?" subtitle="סימון שיחות עוזר לנו לשפר אותו">
      <div className="grid grid-cols-2 gap-2.5">
        <Button variant="good" aria-pressed={verdict === "good"} disabled={saving} onClick={() => save("good")} className="py-3">
          <ThumbsUp className="size-4" aria-hidden />
          תקין
        </Button>
        <Button variant="bad" aria-pressed={verdict === "bad"} disabled={saving} onClick={() => save("bad")} className="py-3">
          <ThumbsDown className="size-4" aria-hidden />
          לא תקין
        </Button>
      </div>
      <label htmlFor="note" className="sr-only">
        הערה
      </label>
      <textarea
        id="note"
        rows={3}
        value={note}
        onChange={(e) => setNote(e.target.value)}
        placeholder="מה השתבש? למשל: שמע את העיר לא נכון"
        className={cx(FIELD, "mt-3 resize-y")}
      />
      <div className="mt-4 border-t border-slate-100 pt-3 dark:border-white/[0.06]">
        <Button variant="ghost" onClick={exportCase} className="-ms-2 text-[13px]">
          <Download className="size-4" aria-hidden />
          הורדה כמקרה בדיקה (eval)
        </Button>
      </div>
    </Card>
  );
}
