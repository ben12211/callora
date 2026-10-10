// The parts of an account's page that WhatsApp and Telegram share: the groups and contacts it
// sends orders to (its "תפוצה"), its queue and the log of messages. The pace keeps the
// service's defaults (8–15 seconds apart, 60 an hour).

import { useEffect, useMemo, useState } from "react";
import { Check, Plus, Search, Send, User, Users, X } from "lucide-react";
import { api, useApi } from "../api";
import { phone } from "../format";
import { Badge, Button, Card, cx, Empty, Loading, Problem, Segmented, When } from "../ui";

export type App = "whatsapp" | "telegram";

type Target = { chat_id: string; chat_name: string; kind: "group" | "contact"; events: string[] };
type Pace = {
  min_delay_s: number;
  max_delay_s: number;
  typing: boolean;
  per_hour: number;
  per_day: number;
  quiet: boolean;
  quiet_from: string;
  quiet_to: string;
  warmup_days: number;
};

export type BroadcastAccount = {
  id: string;
  name: string;
  status: string;
  me: { number: string; name: string; username?: string } | null;
  error: string | null;
  first_ready_at: string | null;
  settings: Pace;
  warming_up: boolean;
  targets: Target[];
  queue: {
    pending: number;
    oldest_pending_minutes: number | null;
    sent_hour: number;
    sent_today: number;
    limit_hour: number;
    limit_day: number;
    quiet_now: boolean;
  };
};
type Chats = { groups: { id: string; name: string }[]; contacts: { id: string; name: string; number: string }[] };
type OutboxRow = {
  id: number;
  account_id: string;
  chat_name: string;
  event: string;
  text: string;
  status: "pending" | "sent" | "failed";
  attempts: number;
  last_error: string | null;
  created_at: string;
  sent_at: string | null;
};

export const EVENT_LABELS: Record<string, string> = {
  order: "הזמנה חדשה",
  order_verify: "הזמנה לבדיקה",
  handoff: "העברה למוקדן",
  test: "בדיקה",
};
const EVENTS = ["order", "order_verify", "handoff"];

export const inputClass =
  "w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm outline-none focus:border-brand-500 focus:ring-2 focus:ring-brand-200 dark:border-slate-700 dark:bg-slate-950 dark:focus:ring-brand-900";

export function QueueSummary({ account: a }: { account: BroadcastAccount }) {
  const q = a.queue;
  return (
    <div className="hidden items-center gap-4 text-xs text-slate-500 md:flex dark:text-slate-400">
      {q.quiet_now && <Badge tone="neutral">שעות שקטות</Badge>}
      <span>
        ממתינות: <b className="tabular-nums text-slate-800 dark:text-slate-200">{q.pending}</b>
      </span>
      <span>
        בשעה: <b className="tabular-nums text-slate-800 dark:text-slate-200">{q.sent_hour}</b>/{q.limit_hour}
      </span>
      <span>
        היום: <b className="tabular-nums text-slate-800 dark:text-slate-200">{q.sent_today}</b>/{q.limit_day}
      </span>
    </div>
  );
}

/** A connected account's body: where it sends orders. */
export function Broadcast({ account: a, onChange }: { account: BroadcastAccount; onChange: () => Promise<void> }) {
  return (
    <>
      <h3 className="mb-1 font-semibold">לאן נשלחות ההזמנות</h3>
      <p className="mb-3 text-sm text-slate-500 dark:text-slate-400">כל נסיעה שהוזמנה נשלחת לכל יעד ברשימה.</p>
      <Targets account={a} onChange={onChange} />
    </>
  );
}

function Targets({ account: a, onChange }: { account: BroadcastAccount; onChange: () => Promise<void> }) {
  const [targets, setTargets] = useState<Target[]>(a.targets);
  const [dirty, setDirty] = useState(false);
  const [picking, setPicking] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  useEffect(() => {
    if (!dirty) setTargets(a.targets);
  }, [a.targets, dirty]);

  const update = (next: Target[]) => {
    setTargets(next);
    setDirty(true);
    setStatus(null);
  };
  const toggleEvent = (i: number, e: string) =>
    update(targets.map((t, j) => (j !== i ? t : { ...t, events: t.events.includes(e) ? t.events.filter((x) => x !== e) : [...t.events, e] })));
  const save = async () => {
    setStatus("שומר…");
    try {
      await api(`/api/whatsapp/accounts/${a.id}/targets`, { method: "PUT", body: JSON.stringify(targets.map((t) => ({ chat_id: t.chat_id, events: t.events }))) });
      setDirty(false);
      setStatus("נשמר");
      await onChange();
    } catch {
      setStatus("לא נשמר: יעד כבר לא קבוצה או איש קשר של החשבון");
    }
  };
  const test = async (chat: string) => {
    setStatus("הודעת בדיקה נכנסה לתור");
    await api(`/api/whatsapp/accounts/${a.id}/test`, { method: "POST", body: JSON.stringify({ chat_id: chat }) }).catch(() => setStatus("שמרו לפני בדיקה"));
  };

  return (
    <>
      {targets.length === 0 ? (
        <Empty>אין עדיין יעדים. הוסיפו קבוצה או איש קשר.</Empty>
      ) : (
        <ul className="divide-y divide-slate-100 dark:divide-slate-800">
          {targets.map((t, i) => (
            <li key={t.chat_id} className="flex flex-wrap items-center gap-3 py-3">
              <span className="flex size-8 items-center justify-center rounded-full bg-slate-100 text-slate-600 dark:bg-slate-800 dark:text-slate-300">
                {t.kind === "group" ? <Users className="size-4" aria-hidden /> : <User className="size-4" aria-hidden />}
              </span>
              <div className="min-w-40 flex-1">
                <div className="font-medium">{t.chat_name}</div>
                <div className="text-xs text-slate-500">{t.kind === "group" ? "קבוצה" : "איש קשר"}</div>
              </div>
              <div className="flex flex-wrap gap-1.5">
                {EVENTS.map((e) => (
                  <button
                    key={e}
                    type="button"
                    aria-pressed={t.events.includes(e)}
                    onClick={() => toggleEvent(i, e)}
                    className="rounded-full border border-slate-300 px-2.5 py-1 text-xs text-slate-600 transition-colors aria-pressed:border-brand-600 aria-pressed:bg-brand-50 aria-pressed:text-brand-800 dark:border-slate-700 dark:text-slate-300 dark:aria-pressed:bg-brand-900/60 dark:aria-pressed:text-brand-100"
                  >
                    {EVENT_LABELS[e]}
                  </button>
                ))}
              </div>
              <Button variant="ghost" onClick={() => test(t.chat_id)} disabled={dirty} title="שלח הודעת בדיקה">
                <Send className="size-4" aria-hidden />
              </Button>
              <Button variant="ghost" onClick={() => update(targets.filter((_, j) => j !== i))} title="הסר">
                <X className="size-4" aria-hidden />
              </Button>
            </li>
          ))}
        </ul>
      )}
      <div className="mt-4 flex flex-wrap items-center gap-3">
        <Button onClick={() => setPicking(true)}>
          <Plus className="size-4" aria-hidden />
          הוסף יעד
        </Button>
        {dirty && (
          <Button variant="primary" onClick={save}>
            שמור תפוצה
          </Button>
        )}
        {status && <span className="text-sm text-slate-500">{status}</span>}
      </div>
      {picking && (
        <Picker
          accountId={a.id}
          taken={new Set(targets.map((t) => t.chat_id))}
          onClose={() => setPicking(false)}
          onPick={(picked) => {
            update([...targets, ...picked.map((p) => ({ ...p, events: ["order", "order_verify"] }))]);
            setPicking(false);
          }}
        />
      )}
    </>
  );
}

/** A contact's phone number, or its @name (a Telegram contact may have no number). */
function contactSub(number: string): string {
  return !number || number.startsWith("@") ? number : phone(`+${number}`);
}

/** Only the account's groups and saved contacts: nothing typed in by hand. */
function Picker({ accountId, taken, onClose, onPick }: { accountId: string; taken: Set<string>; onClose: () => void; onPick: (p: Omit<Target, "events">[]) => void }) {
  const [kind, setKind] = useState<"group" | "contact">("group");
  const [query, setQuery] = useState("");
  const [chosen, setChosen] = useState<Map<string, Omit<Target, "events">>>(new Map());
  const chats = useApi<Chats>(`/api/whatsapp/accounts/${accountId}/chats`);

  const list = useMemo(() => {
    const q = query.trim().toLowerCase();
    const items =
      kind === "group"
        ? (chats.data?.groups ?? []).map((g) => ({ id: g.id, name: g.name, sub: "" }))
        : (chats.data?.contacts ?? []).map((c) => ({ id: c.id, name: c.name, sub: contactSub(c.number) }));
    return items.filter((i) => !taken.has(i.id) && (!q || i.name.toLowerCase().includes(q) || i.sub.replace(/\D/g, "").includes(q.replace(/\D/g, "") || "~")));
  }, [chats.data, kind, query, taken]);

  const toggle = (id: string, name: string) =>
    setChosen((m) => {
      const next = new Map(m);
      if (next.has(id)) next.delete(id);
      else next.set(id, { chat_id: id, chat_name: name, kind });
      return next;
    });

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4" role="dialog" aria-modal="true" aria-label="בחירת יעדים">
      <button type="button" className="absolute inset-0 bg-slate-950/50" aria-label="סגירה" onClick={onClose} />
      <div className="relative flex max-h-[85dvh] w-full max-w-lg flex-col rounded-xl bg-white shadow-xl dark:bg-slate-900">
        <header className="flex items-center justify-between border-b border-slate-100 px-5 py-4 dark:border-slate-800">
          <h2 className="font-semibold">הוספת יעדים</h2>
          <button type="button" onClick={onClose} aria-label="סגירה" className="rounded-lg p-1.5 hover:bg-slate-100 dark:hover:bg-slate-800">
            <X className="size-4" aria-hidden />
          </button>
        </header>
        <div className="flex flex-col gap-3 px-5 pt-4">
          <Segmented
            label="סוג"
            value={kind}
            options={[
              { value: "group", label: `קבוצות (${chats.data?.groups.length ?? "…"})` },
              { value: "contact", label: `אנשי קשר (${chats.data?.contacts.length ?? "…"})` },
            ]}
            onChange={setKind}
          />
          <div className="relative">
            <Search className="pointer-events-none absolute inset-y-0 start-3 my-auto size-4 text-slate-400" aria-hidden />
            <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="חיפוש" aria-label="חיפוש" className={cx(inputClass, "ps-9")} />
          </div>
        </div>
        <div className="mt-3 flex-1 overflow-y-auto px-2">
          {chats.error ? (
            <div className="px-3">
              <Problem>הרשימה לא נטענה. ודאו שהחשבון מחובר.</Problem>
            </div>
          ) : !chats.data ? (
            <Loading label="טוען קבוצות ואנשי קשר…" />
          ) : list.length === 0 ? (
            <Empty>{kind === "group" ? "אין קבוצות מתאימות" : "אין אנשי קשר שמורים מתאימים"}</Empty>
          ) : (
            <ul>
              {list.map((i) => {
                const on = chosen.has(i.id);
                return (
                  <li key={i.id}>
                    <button
                      type="button"
                      onClick={() => toggle(i.id, i.name)}
                      className={cx("flex w-full items-center gap-3 rounded-lg px-3 py-2.5 text-start hover:bg-slate-50 dark:hover:bg-slate-800", on && "bg-brand-50 dark:bg-brand-900/40")}
                    >
                      <span className={cx("flex size-5 items-center justify-center rounded border", on ? "border-brand-600 bg-brand-600 text-white" : "border-slate-300 dark:border-slate-600")}>
                        {on && <Check className="size-3.5" aria-hidden />}
                      </span>
                      <span className="flex-1 font-medium">{i.name}</span>
                      {i.sub && <span className="ltr text-xs tabular-nums text-slate-500">{i.sub}</span>}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
        <footer className="flex items-center justify-between gap-3 border-t border-slate-100 px-5 py-3 dark:border-slate-800">
          <span className="text-sm text-slate-500">{chosen.size ? `${chosen.size} נבחרו` : "אפשר לבחור כמה"}</span>
          <Button variant="primary" disabled={!chosen.size} onClick={() => onPick([...chosen.values()])}>
            הוסף לתפוצה
          </Button>
        </footer>
      </div>
    </div>
  );
}

export function Log({ app, accounts }: { app: App; accounts: BroadcastAccount[] }) {
  const log = useApi<OutboxRow[]>(`/api/whatsapp/outbox?limit=100&app=${app}`, 10000);
  const names = new Map(accounts.map((a) => [a.id, a.name]));
  const retry = async (id: number) => {
    await api(`/api/whatsapp/outbox/${id}/retry`, { method: "POST" });
    await log.reload();
  };
  return (
    <Card title="יומן הודעות" className="mt-6">
      {log.error ? (
        <Problem>{log.error}</Problem>
      ) : !log.data ? (
        <Loading />
      ) : log.data.length === 0 ? (
        <Empty>עוד לא נשלחו הודעות</Empty>
      ) : (
        <div className="-mx-5 overflow-x-auto">
          <table className="w-full min-w-[760px] text-sm">
            <thead>
              <tr className="border-y border-slate-100 bg-slate-50 text-xs text-slate-500 dark:border-slate-800 dark:bg-slate-900/60 dark:text-slate-400">
                <th className="px-5 py-2.5 text-start font-medium">זמן</th>
                <th className="px-3 py-2.5 text-start font-medium">חשבון</th>
                <th className="px-3 py-2.5 text-start font-medium">יעד</th>
                <th className="px-3 py-2.5 text-start font-medium">סוג</th>
                <th className="px-3 py-2.5 text-start font-medium">מצב</th>
                <th className="px-5 py-2.5 text-start font-medium"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-slate-100 dark:divide-slate-800">
              {log.data.map((m) => (
                <tr key={m.id} title={m.text}>
                  <td className="whitespace-nowrap px-5 py-2.5 text-slate-600 dark:text-slate-300">
                    <When iso={m.sent_at ?? m.created_at} />
                  </td>
                  <td className="px-3 py-2.5">{names.get(m.account_id) ?? m.account_id}</td>
                  <td className="px-3 py-2.5 font-medium">{m.chat_name}</td>
                  <td className="px-3 py-2.5 text-slate-600 dark:text-slate-300">{EVENT_LABELS[m.event] ?? m.event}</td>
                  <td className="px-3 py-2.5">
                    {m.status === "sent" ? (
                      <Badge tone="good">נשלחה</Badge>
                    ) : m.status === "pending" ? (
                      <Badge tone="brand">{m.attempts ? `ממתינה (ניסיון ${m.attempts + 1})` : "ממתינה"}</Badge>
                    ) : (
                      <Badge tone="bad">נכשלה</Badge>
                    )}
                    {m.last_error && <div className="mt-0.5 text-xs text-slate-500">{m.last_error}</div>}
                  </td>
                  <td className="px-5 py-2.5 text-end">
                    {m.status === "failed" && (
                      <Button variant="ghost" onClick={() => retry(m.id)}>
                        שלח שוב
                      </Button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  );
}
