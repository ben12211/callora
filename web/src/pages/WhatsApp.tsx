import { useEffect, useMemo, useState } from "react";
import { Check, Plus, RefreshCw, Search, Send, ShieldAlert, Trash2, User, Users, X } from "lucide-react";
import { useSearchParams } from "react-router-dom";
import { api, useApi } from "../api";
import { phone } from "../format";
import { Badge, Button, Card, cx, Empty, Loading, PageHeader, Problem, Segmented, When } from "../ui";
import { Telegram } from "./Telegram";

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
type Account = {
  id: string;
  name: string;
  status: string;
  me: { number: string; name: string } | null;
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
export type WaOverview = { configured: boolean; reachable?: boolean; max?: number; accounts?: Account[] };
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

const STATUS: Record<string, { label: string; tone: "good" | "warn" | "bad" | "neutral" | "brand" }> = {
  ready: { label: "מחובר", tone: "good" },
  qr: { label: "ממתין לסריקה", tone: "brand" },
  starting: { label: "מתחבר…", tone: "neutral" },
  authenticating: { label: "מתחבר…", tone: "neutral" },
  disconnected: { label: "מנותק", tone: "warn" },
  failed: { label: "שגיאה", tone: "bad" },
};

/** An account that needs attention, for the overview's alert. */
export function whatsappTrouble(o: WaOverview | null): string | null {
  if (!o?.configured) return null;
  if (o.reachable === false) return "שירות הוואטסאפ לא זמין";
  const down = (o.accounts ?? []).filter((a) => a.status === "disconnected" || a.status === "failed");
  if (down.length) return `חשבון וואטסאפ מנותק: ${down.map((a) => a.name).join(", ")}`;
  const stuck = (o.accounts ?? []).filter((a) => (a.queue.oldest_pending_minutes ?? 0) > 10);
  if (stuck.length) return `הודעות וואטסאפ ממתינות יותר מ-10 דקות: ${stuck.map((a) => a.name).join(", ")}`;
  return null;
}

/** The accounts the business sends and asks from: WhatsApp (orders, and the price bot if it is
 * there) and Telegram (the price bot), each signed in by QR. */
export function WhatsApp() {
  const [params, setParams] = useSearchParams();
  const app = params.get("app") === "telegram" ? "telegram" : "whatsapp";
  return (
    <>
      <div className="mb-6">
        <Segmented
          label="אפליקציה"
          value={app}
          options={[
            { value: "whatsapp", label: "וואטסאפ" },
            { value: "telegram", label: "טלגרם" },
          ]}
          onChange={(v) => setParams(v === "telegram" ? { app: "telegram" } : {})}
        />
      </div>
      {app === "telegram" ? <Telegram /> : <WhatsAppAccounts />}
    </>
  );
}

function WhatsAppAccounts() {
  // Often while an account is connecting (the QR changes), calmly once all are connected.
  const [fast, setFast] = useState(true);
  const overview = useApi<WaOverview>("/api/whatsapp", fast ? 4000 : 15000);
  const o = overview.data;
  const accounts = useMemo(() => o?.accounts ?? [], [o]);
  useEffect(() => {
    if (o) setFast(accounts.some((a) => a.status !== "ready"));
  }, [o, accounts]);

  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const add = async () => {
    await api("/api/whatsapp/accounts", { method: "POST", body: JSON.stringify({ name: name.trim() || "חשבון" }) });
    setName("");
    setAdding(false);
    setFast(true);
    await overview.reload();
  };

  const full = o?.max != null && accounts.length >= o.max;

  return (
    <>
      <PageHeader
        title="וואטסאפ"
        subtitle="כל הזמנה נשלחת לקבוצות ולאנשי הקשר שבחרתם, בקצב של אדם"
        action={
          o?.configured && o.reachable ? (
            <Button variant="primary" onClick={() => setAdding(true)} disabled={full} title={full ? `עד ${o.max} חשבונות` : undefined}>
              <Plus className="size-4" aria-hidden />
              הוסף חשבון
            </Button>
          ) : undefined
        }
      />

      {overview.error && <Problem>{overview.error}</Problem>}
      {!o && !overview.error && <Loading />}
      {o && !o.configured && (
        <Card>
          <Empty>שירות הוואטסאפ לא מוגדר בשרת (WHATSAPP_URL).</Empty>
        </Card>
      )}
      {o?.configured && o.reachable === false && <Problem>שירות הוואטסאפ לא זמין כרגע. ההזמנות נשמרות בתור ויישלחו כשיחזור.</Problem>}

      {o?.configured && o.reachable && (
        <>
          <div className="mb-6 flex gap-3 rounded-xl border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-900 dark:border-amber-900 dark:bg-amber-950/40 dark:text-amber-200">
            <ShieldAlert className="mt-0.5 size-4 shrink-0" aria-hidden />
            <p>
              החיבור עובד כמו וואטסאפ ווב ואינו רשמי, ולכן מספר עלול להיחסם. כדאי לחבר מספר נפרד ולא את המספר הפרטי. ההודעות נשלחות רק לקבוצות שהמספר חבר בהן ולאנשי קשר
              שמורים, בקצב שמוגדר לכל חשבון.
            </p>
          </div>

          {adding && (
            <Card title="חשבון חדש" className="mb-6">
              <div className="flex flex-wrap items-end gap-3">
                <div className="min-w-56 flex-1">
                  <label htmlFor="acc-name" className="mb-1.5 block text-sm font-medium">
                    שם החשבון
                  </label>
                  <input
                    id="acc-name"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    placeholder="למשל: מוקד ראשי"
                    autoFocus
                    className={inputClass}
                    onKeyDown={(e) => e.key === "Enter" && void add()}
                  />
                </div>
                <Button variant="primary" onClick={add}>
                  צור והצג קוד QR
                </Button>
                <Button variant="ghost" onClick={() => setAdding(false)}>
                  ביטול
                </Button>
              </div>
            </Card>
          )}

          {accounts.length === 0 && !adding ? (
            <Card>
              <Empty>עוד אין חשבון מחובר. לחצו ״הוסף חשבון״ וסרקו את הקוד מהטלפון.</Empty>
            </Card>
          ) : (
            <div className="flex flex-col gap-6">
              {accounts.map((a) => (
                <AccountCard key={a.id} account={a} onChange={overview.reload} />
              ))}
            </div>
          )}

          <Log accounts={accounts} />
        </>
      )}
    </>
  );
}

const inputClass =
  "w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm outline-none focus:border-brand-500 focus:ring-2 focus:ring-brand-200 dark:border-slate-700 dark:bg-slate-950 dark:focus:ring-brand-900";

function AccountCard({ account: a, onChange }: { account: Account; onChange: () => Promise<void> }) {
  const status = STATUS[a.status] ?? { label: a.status, tone: "neutral" as const };
  const [tab, setTab] = useState<"targets" | "pace">("targets");
  const [confirmDelete, setConfirmDelete] = useState(false);

  const remove = async () => {
    await api(`/api/whatsapp/accounts/${a.id}`, { method: "DELETE" });
    await onChange();
  };
  const restart = async () => {
    await api(`/api/whatsapp/accounts/${a.id}/restart`, { method: "POST" });
    await onChange();
  };

  return (
    <section className="rounded-xl border border-slate-200 bg-white shadow-sm dark:border-slate-800 dark:bg-slate-900">
      <header className="flex flex-wrap items-center gap-3 border-b border-slate-100 px-5 py-4 dark:border-slate-800">
        <div className="flex size-10 items-center justify-center rounded-full bg-emerald-600 text-white">
          <Send className="size-4" aria-hidden />
        </div>
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h2 className="font-semibold">{a.name}</h2>
            <Badge tone={status.tone}>{status.label}</Badge>
            {a.status === "ready" && a.warming_up && <Badge tone="warn">חימום: חצי מהמכסה</Badge>}
          </div>
          <div className="text-sm text-slate-500 dark:text-slate-400">
            {a.me ? (
              <>
                <span className="ltr tabular-nums">{phone(`+${a.me.number}`)}</span>
                {a.me.name && ` · ${a.me.name}`}
              </>
            ) : (
              "לא מחובר למספר"
            )}
          </div>
        </div>
        <div className="ms-auto flex items-center gap-2">
          {a.status === "ready" && <QueueSummary account={a} />}
          <Button variant="ghost" onClick={restart} title="חבר מחדש">
            <RefreshCw className="size-4" aria-hidden />
          </Button>
          {confirmDelete ? (
            <>
              <Button variant="bad" onClick={remove}>
                כן, נתק ומחק
              </Button>
              <Button variant="ghost" onClick={() => setConfirmDelete(false)}>
                ביטול
              </Button>
            </>
          ) : (
            <Button variant="ghost" onClick={() => setConfirmDelete(true)} title="נתק ומחק">
              <Trash2 className="size-4" aria-hidden />
            </Button>
          )}
        </div>
      </header>

      <div className="p-5">
        {a.status !== "ready" ? (
          <Connect account={a} />
        ) : (
          <>
            <Segmented
              label="אזור"
              value={tab}
              options={[
                { value: "targets", label: `תפוצה (${a.targets.length})` },
                { value: "pace", label: "קצב שליחה" },
              ]}
              onChange={setTab}
            />
            <div className="mt-5">{tab === "targets" ? <Targets account={a} onChange={onChange} /> : <PaceForm account={a} onChange={onChange} />}</div>
          </>
        )}
      </div>
    </section>
  );
}

function QueueSummary({ account: a }: { account: Account }) {
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

function Connect({ account: a }: { account: Account }) {
  const [qr, setQr] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    const load = () =>
      api<{ qr: string | null }>(`/api/whatsapp/accounts/${a.id}/qr`)
        .then((r) => alive && setQr(r.qr))
        .catch(() => undefined);
    void load();
    const t = window.setInterval(load, 3000);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, [a.id]);

  return (
    <div className="flex flex-wrap items-center gap-8">
      <div className="flex size-64 items-center justify-center rounded-xl border border-slate-200 bg-white p-3 dark:border-slate-700">
        {qr ? <img src={qr} alt="קוד QR לחיבור וואטסאפ" className="size-full" /> : <Loading label={a.status === "failed" ? "לא עלה" : "מכין קוד…"} />}
      </div>
      <div className="max-w-sm text-sm leading-relaxed text-slate-600 dark:text-slate-300">
        <h3 className="mb-2 font-semibold text-slate-900 dark:text-white">חיבור המספר</h3>
        <ol className="list-decimal space-y-1 ps-5">
          <li>פתחו את וואטסאפ בטלפון של המספר שמתחברים איתו.</li>
          <li>הגדרות ← מכשירים מקושרים ← קישור מכשיר.</li>
          <li>סרקו את הקוד. הוא מתחלף לבד כל כמה שניות.</li>
        </ol>
        {a.status === "disconnected" && <p className="mt-3 text-amber-700 dark:text-amber-400">החשבון התנתק. סרקו שוב, או לחצו ״חבר מחדש״.</p>}
        {a.status === "failed" && a.error && <p className="mt-3 text-rose-600 dark:text-rose-400">שגיאה: {a.error}</p>}
      </div>
    </div>
  );
}

function Targets({ account: a, onChange }: { account: Account; onChange: () => Promise<void> }) {
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
        : (chats.data?.contacts ?? []).map((c) => ({ id: c.id, name: c.name, sub: phone(`+${c.number}`) }));
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

function PaceForm({ account: a, onChange }: { account: Account; onChange: () => Promise<void> }) {
  const [p, setP] = useState<Pace>(a.settings);
  const [status, setStatus] = useState<string | null>(null);
  const set = <K extends keyof Pace>(k: K, v: Pace[K]) => {
    setP((old) => ({ ...old, [k]: v }));
    setStatus(null);
  };
  const save = async () => {
    setStatus("שומר…");
    try {
      await api(`/api/whatsapp/accounts/${a.id}/settings`, { method: "PUT", body: JSON.stringify(p) });
      setStatus("נשמר");
      await onChange();
    } catch {
      setStatus("לא נשמר: בדקו את הערכים");
    }
  };
  const num = (k: keyof Pace, label: string, hint: string, min: number, max: number) => (
    <label className="flex flex-col gap-1.5">
      <span className="text-sm font-medium">{label}</span>
      <input type="number" min={min} max={max} value={p[k] as number} onChange={(e) => set(k, Number(e.target.value) as never)} className={cx(inputClass, "tabular-nums")} />
      <span className="text-xs text-slate-500">{hint}</span>
    </label>
  );
  return (
    <div className="flex flex-col gap-6">
      <div className="grid gap-5 sm:grid-cols-2 lg:grid-cols-3">
        {num("min_delay_s", "המתנה מינימלית בין הודעות (שניות)", "ההמתנה נבחרת באקראי בין המינימום למקסימום", 3, 600)}
        {num("max_delay_s", "המתנה מקסימלית בין הודעות (שניות)", "ברירת מחדל 8–15", 3, 900)}
        {num("per_hour", "מקסימום הודעות לשעה", "מעבר לזה ההודעות מחכות בתור", 1, 500)}
        {num("per_day", "מקסימום הודעות ליום", "נספר מחצות, שעון ישראל", 1, 5000)}
        {num("warmup_days", "ימי חימום למספר חדש", "בימים האלה המכסות בחצי", 0, 30)}
      </div>
      <div className="flex flex-col gap-4 border-t border-slate-100 pt-5 dark:border-slate-800">
        <label className="flex items-center gap-3 text-sm">
          <input type="checkbox" checked={p.typing} onChange={(e) => set("typing", e.target.checked)} className="size-4 accent-brand-700" />
          ״מקליד…״ לפני כל הודעה (1–3 שניות)
        </label>
        <div className="flex flex-wrap items-center gap-3 text-sm">
          <label className="flex items-center gap-3">
            <input type="checkbox" checked={p.quiet} onChange={(e) => set("quiet", e.target.checked)} className="size-4 accent-brand-700" />
            שעות שקטות, בלי שליחה בין
          </label>
          <input type="time" value={p.quiet_from} onChange={(e) => set("quiet_from", e.target.value)} disabled={!p.quiet} className={cx(inputClass, "w-32")} aria-label="מתחילות ב" />
          <span>ל-</span>
          <input type="time" value={p.quiet_to} onChange={(e) => set("quiet_to", e.target.value)} disabled={!p.quiet} className={cx(inputClass, "w-32")} aria-label="נגמרות ב" />
        </div>
      </div>
      <div className="flex items-center gap-3">
        <Button variant="primary" onClick={save}>
          שמור קצב
        </Button>
        {status && <span className="text-sm text-slate-500">{status}</span>}
      </div>
    </div>
  );
}

function Log({ accounts }: { accounts: Account[] }) {
  const log = useApi<OutboxRow[]>("/api/whatsapp/outbox?limit=100", 10000);
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
