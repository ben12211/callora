import { useEffect, useMemo, useState } from "react";
import { Plus, RefreshCw, Send, ShieldAlert, Trash2 } from "lucide-react";
import { useSearchParams } from "react-router-dom";
import { api, useApi } from "../api";
import { phone } from "../format";
import { Badge, Button, Card, Empty, Loading, PageHeader, Problem, Segmented } from "../ui";
import { Broadcast, type BroadcastAccount, inputClass, Log, QueueSummary } from "./broadcast";
import { Telegram } from "./Telegram";

export type WaOverview = { configured: boolean; reachable?: boolean; max?: number; accounts?: BroadcastAccount[] };

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

          <Log app="whatsapp" accounts={accounts} />
        </>
      )}
    </>
  );
}

function AccountCard({ account: a, onChange }: { account: BroadcastAccount; onChange: () => Promise<void> }) {
  const status = STATUS[a.status] ?? { label: a.status, tone: "neutral" as const };
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
          <Broadcast account={a} onChange={onChange} />
        )}
      </div>
    </section>
  );
}

function Connect({ account: a }: { account: BroadcastAccount }) {
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
