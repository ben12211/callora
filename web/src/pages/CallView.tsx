import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { ArrowRight, Download, Headset, TriangleAlert } from "lucide-react";
import { api, type CallDetail, type Turn, useApi, type Verdict } from "../api";
import { ACTIONS, clock, duration, phone, secondsOf, SLOTS, tokens } from "../format";
import { Badge, Button, Card, cx, Empty, Loading, Problem, When } from "../ui";
import { OutcomeBadge } from "./Calls";

export function CallView() {
  const { id = "" } = useParams();
  const call = useApi<CallDetail>(`/api/calls/${encodeURIComponent(id)}`);
  const c = call.data;

  const back = (
    <Link to="/calls" className="mb-4 inline-flex items-center gap-1.5 text-sm font-medium text-brand-700 hover:underline dark:text-brand-300">
      <ArrowRight className="size-4" aria-hidden />
      כל השיחות
    </Link>
  );

  if (call.error) return (<>{back}<Problem>{call.error === "הנתונים לא נטענו" ? "השיחה לא נמצאה" : call.error}</Problem></>);
  if (!c) return (<>{back}<Loading /></>);

  const seconds = c.ended_at ? (new Date(c.ended_at).getTime() - new Date(c.started_at).getTime()) / 1000 : null;

  return (
    <>
      {back}
      <div className="mb-6 flex flex-wrap items-start justify-between gap-4 rounded-xl border border-slate-200 bg-white p-5 shadow-sm dark:border-slate-800 dark:bg-slate-900">
        <div>
          <div className="flex flex-wrap items-center gap-3">
            <h1 className="ltr text-xl font-semibold tabular-nums">{phone(c.from)}</h1>
            <OutcomeBadge outcome={c.outcome} />
          </div>
          <p className="mt-1 text-sm text-slate-500 dark:text-slate-400">
            <When iso={c.started_at} /> · משך {duration(seconds)}
          </p>
        </div>
        {c.usage && (
          <div className="text-sm text-slate-500 dark:text-slate-400">
            <div className="font-medium text-slate-700 dark:text-slate-200">{c.usage.model}</div>
            <div className="tabular-nums">
              טוקנים: {tokens(c.usage.input)} נכנסים, מהם {tokens(c.usage.cached)} מהמטמון · {tokens(c.usage.output)} יוצאים
            </div>
          </div>
        )}
      </div>

      <div className="grid gap-6 lg:grid-cols-3">
        <Card title="השיחה" className="lg:col-span-2">
          {c.turns.length === 0 ? (
            <Empty>התמליל נמחק (לפי זמן השמירה) או שהשיחה ריקה</Empty>
          ) : (
            <ol className="flex flex-col gap-3">
              {c.turns.map((t, i) => (
                <TurnView key={i} turn={t} />
              ))}
            </ol>
          )}
        </Card>

        <div className="flex flex-col gap-6">
          <Review call={c} />
          {c.utterances.length > 0 && (
            <Card title="הקלטות המתקשר">
              <p className="-mt-1 mb-3 text-xs text-slate-500">נשמרות רק ממספרי בדיקה.</p>
              <ul className="flex flex-col gap-3">
                {c.utterances.map((u) => (
                  <li key={u.id}>
                    <div className="mb-1 text-xs text-slate-600 dark:text-slate-300">{u.heard}</div>
                    <audio controls preload="none" src={`/api/utterances/${u.id}`} className="h-9 w-full" />
                  </li>
                ))}
              </ul>
            </Card>
          )}
          {c.actions.length > 0 && (
            <Card title="פעולות">
              <ul className="flex flex-col gap-2">
                {c.actions.map((a, i) => {
                  const unknown = a.result?.error?.outcome_unknown;
                  return (
                    <li key={i} className="flex items-center gap-2 text-sm">
                      <span className="font-medium">{a.action}</span>
                      <Badge tone={a.ok ? "good" : unknown ? "warn" : "bad"}>{a.ok ? "הצליח" : unknown ? "לא ידוע אם נקלט" : "נכשל"}</Badge>
                      <span className="ms-auto text-xs tabular-nums text-slate-500">{secondsOf(a.latency_ms)}</span>
                    </li>
                  );
                })}
              </ul>
            </Card>
          )}
          {c.handoffs.map((h, i) => (
            <Card key={i} title={<span className="inline-flex items-center gap-2"><Headset className="size-4" aria-hidden />הועבר למוקדן</span>}>
              <div className="text-xs text-slate-500">{h.reason}</div>
              {h.summary?.text && <p className="mt-2 text-sm">{h.summary.text}</p>}
            </Card>
          ))}
        </div>
      </div>
    </>
  );
}

function TurnView({ turn }: { turn: Turn }) {
  const agent = turn.speaker === "agent";
  const d = turn.detail ?? {};
  const r = d.route === "agent" ? d.reply ?? {} : null;
  const chips: string[] = [];
  if (r) {
    chips.push("סוכן");
    if (d.decision_ms != null) chips.push(secondsOf(d.decision_ms));
    if (r.action && ACTIONS[r.action]) chips.push(ACTIONS[r.action]);
    if (r.phrase) chips.push(`הקלטה: ${r.phrase}`);
    for (const f of r.fields ?? []) chips.push(`${SLOTS[f.slot] ?? f.slot}: ${f.value}`);
  } else if (!agent && d.transcript != null) {
    chips.push("נענה בחוקים, בלי הסוכן");
  }
  return (
    <li className={cx("flex flex-col", agent ? "items-start" : "items-end")}>
      <div
        className={cx(
          "max-w-[85%] rounded-2xl px-4 py-2.5 text-sm leading-relaxed",
          agent
            ? "rounded-ss-sm bg-brand-50 text-slate-900 dark:bg-brand-900/50 dark:text-slate-100"
            : "rounded-se-sm bg-slate-100 text-slate-900 dark:bg-slate-800 dark:text-slate-100",
        )}
      >
        <div className="mb-0.5 flex items-center gap-2 text-[11px] text-slate-500 dark:text-slate-400">
          <span className="font-medium">{agent ? "קלורה" : "מתקשר"}</span>
          <span className="tabular-nums">{clock(turn.at)}</span>
        </div>
        {turn.text}
      </div>
      {chips.length > 0 && <div className="mt-1 max-w-[85%] text-[11px] text-slate-500 dark:text-slate-400">{chips.join(" · ")}</div>}
      {d.second_hearing && <div className="mt-0.5 text-[11px] text-slate-500">שמיעה שנייה: {d.second_hearing}</div>}
      {d.held_for_open_question && (
        <div className="mt-1 inline-flex items-center gap-1 text-[11px] text-amber-700 dark:text-amber-400">
          <TriangleAlert className="size-3" aria-hidden />
          עבר לשאלה אחרת בלי תשובה על {SLOTS[d.held_for_open_question] ?? d.held_for_open_question}: נשאל שוב
        </div>
      )}
      {d.error && <div className="mt-1 text-[11px] text-rose-600 dark:text-rose-400">שגיאה: {d.error}</div>}
    </li>
  );
}

function Review({ call }: { call: CallDetail }) {
  const [verdict, setVerdict] = useState<Verdict | null>(call.review?.verdict ?? null);
  const [note, setNote] = useState(call.review?.note ?? "");
  const [status, setStatus] = useState<string | null>(null);

  useEffect(() => {
    setVerdict(call.review?.verdict ?? null);
    setNote(call.review?.note ?? "");
  }, [call.id, call.review]);

  const save = async (v: Verdict) => {
    setStatus("שומר…");
    try {
      await api(`/api/calls/${call.id}/review`, { method: "PUT", body: JSON.stringify({ verdict: v, note }) });
      setVerdict(v);
      setStatus("נשמר");
    } catch {
      setStatus("לא נשמר");
    }
  };

  const exportCase = async () => {
    const data = await api<unknown>(`/api/calls/${call.id}/eval-case`);
    const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2) + "\n"], { type: "application/json" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `call_${call.call_sid || call.id}.json`;
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <Card title="בדיקה">
      <label htmlFor="note" className="sr-only">
        הערה
      </label>
      <textarea
        id="note"
        rows={3}
        value={note}
        onChange={(e) => setNote(e.target.value)}
        placeholder="מה השתבש? למשל: שמע את העיר לא נכון"
        className="w-full resize-y rounded-lg border border-slate-300 bg-white p-2.5 text-sm outline-none focus:border-brand-500 focus:ring-2 focus:ring-brand-200 dark:border-slate-700 dark:bg-slate-950 dark:focus:ring-brand-900"
      />
      <div className="mt-3 flex items-center gap-2">
        <Button variant="good" aria-pressed={verdict === "good"} onClick={() => save("good")}>
          תקין
        </Button>
        <Button variant="bad" aria-pressed={verdict === "bad"} onClick={() => save("bad")}>
          לא תקין
        </Button>
        {status && <span className="text-xs text-slate-500">{status}</span>}
      </div>
      <div className="mt-4 border-t border-slate-100 pt-4 dark:border-slate-800">
        <Button variant="ghost" onClick={exportCase} className="-ms-2">
          <Download className="size-4" aria-hidden />
          הורדה כמקרה בדיקה (eval)
        </Button>
      </div>
    </Card>
  );
}
