// Telegram accounts: signed in by scanning a QR code from the Telegram app (and the two-step
// password, when the account has one), like WhatsApp's. They ask the price-list bot, which
// answers on Telegram only, and, like WhatsApp accounts, send every new order to the groups and
// contacts picked here, at a human pace. An account can make an orders group (only it and its
// bot write there) and hand the sending of orders to a bot of its own.

import { useEffect, useMemo, useState } from "react";
import { Bot, Copy, KeyRound, Plus, RefreshCw, Send, Trash2, Users } from "lucide-react";
import { Link } from "react-router-dom";
import { api, Unauthorized, useApi } from "../api";
import { phone } from "../format";
import { Badge, Button, Card, Empty, Input, Loading, PageHeader, Problem, Segmented, useToast } from "../ui";
import { Broadcast, type BroadcastAccount, Log, QueueSummary } from "./broadcast";

export type TgAccount = {
  id: string;
  name: string;
  status: "starting" | "qr" | "password" | "ready" | "disconnected" | "failed";
  me: { number: string; name: string; username: string } | null;
  hint: string | null;
  error: string | null;
  first_ready_at: string | null;
  bot?: { username: string } | null;
  via_bot?: boolean;
  groups?: { id: string; name: string; link: string }[];
} & Partial<Pick<BroadcastAccount, "settings" | "warming_up" | "targets" | "queue">>;
export type TgOverview = { service: boolean; reachable?: boolean; configured?: boolean; max?: number; accounts?: TgAccount[] };

const STATUS: Record<string, { label: string; tone: "good" | "warn" | "bad" | "neutral" | "brand" }> = {
  ready: { label: "מחובר", tone: "good" },
  qr: { label: "ממתין לסריקה", tone: "brand" },
  password: { label: "ממתין לסיסמה", tone: "brand" },
  starting: { label: "מתחבר…", tone: "neutral" },
  disconnected: { label: "מנותק", tone: "warn" },
  failed: { label: "שגיאה", tone: "bad" },
};

/** The server sent the account's targets, pace and queue (it has a database). */
function isBroadcast(a: TgAccount): a is TgAccount & BroadcastAccount {
  return Boolean(a.settings && a.targets && a.queue);
}

/** "+972 52-…" or "@name", whichever the account has. */
export function tgWho(me: TgAccount["me"]): string {
  if (!me) return "";
  return me.number ? phone(`+${me.number}`) : me.username ? `@${me.username}` : "";
}

export function Telegram() {
  // Often while an account is signing in (the QR changes every half minute), calmly after.
  const [fast, setFast] = useState(true);
  const overview = useApi<TgOverview>("/api/telegram", fast ? 3000 : 15000);
  const o = overview.data;
  const accounts = useMemo(() => o?.accounts ?? [], [o]);
  useEffect(() => {
    if (o) setFast(accounts.some((a) => a.status !== "ready"));
  }, [o, accounts]);

  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const add = async () => {
    setError(null);
    try {
      await api("/api/telegram/accounts", { method: "POST", body: JSON.stringify({ name: name.trim() || "טלגרם" }) });
      setName("");
      setAdding(false);
      setFast(true);
      await overview.reload();
    } catch (e) {
      if (!(e instanceof Unauthorized)) setError("לא נוצר. נסו שוב.");
    }
  };
  const full = o?.max != null && accounts.length >= o.max;
  const ready = o?.service && o.reachable && o.configured;

  return (
    <>
      <PageHeader
        title="טלגרם"
        subtitle="כל הזמנה נשלחת לקבוצות ולאנשי הקשר שבחרתם, בקצב של אדם. החשבון גם שואל את בוט המחירון."
        action={
          ready ? (
            <Button variant="primary" onClick={() => setAdding(true)} disabled={full} title={full ? `עד ${o?.max} חשבונות` : undefined}>
              <Plus className="size-4" aria-hidden />
              חבר חשבון טלגרם
            </Button>
          ) : undefined
        }
      />

      {overview.error && <Problem>{overview.error}</Problem>}
      {!o && !overview.error && <Loading />}
      {o && !o.service && (
        <Card>
          <Empty>שירות ההודעות לא מוגדר בשרת.</Empty>
        </Card>
      )}
      {o?.service && o.reachable === false && <Problem>שירות ההודעות לא זמין כרגע.</Problem>}
      {o?.service && o.reachable && o.configured === false && <Setup />}

      {ready && (
        <>
          {adding && (
            <Card title="חשבון טלגרם חדש" className="mb-6">
              <div className="flex flex-wrap items-end gap-3">
                <div className="min-w-56 flex-1">
                  <label htmlFor="tg-name" className="mb-1.5 block text-sm font-medium">
                    שם החשבון
                  </label>
                  <Input
                    id="tg-name"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    placeholder="למשל: מחירון"
                    autoFocus
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
              {error && (
                <div className="mt-3">
                  <Problem>{error}</Problem>
                </div>
              )}
            </Card>
          )}

          {accounts.length === 0 && !adding ? (
            <Card>
              <Empty icon={<Send className="size-5" aria-hidden />}>
                עוד אין חשבון טלגרם. לחצו ״חבר חשבון טלגרם״ וסרקו את הקוד מאפליקציית טלגרם בטלפון.
              </Empty>
            </Card>
          ) : (
            <div className="flex flex-col gap-6">
              {accounts.map((a) => (
                <AccountCard key={a.id} account={a} onChange={overview.reload} />
              ))}
            </div>
          )}
          {accounts.some((a) => a.status === "ready") && <Log app="telegram" accounts={accounts.filter(isBroadcast)} />}
        </>
      )}
    </>
  );
}

/** What the server needs before any account can sign in: Telegram's API id and hash. */
function Setup() {
  return (
    <Card title="צריך להגדיר את טלגרם פעם אחת">
      <ol className="list-decimal space-y-2 ps-5 text-sm leading-relaxed text-slate-600 dark:text-slate-300">
        <li>
          היכנסו ל-<span dir="ltr">my.telegram.org</span> עם מספר הטלפון של החשבון ששואל את הבוט.
        </li>
        <li>
          ״API development tools״ ← צרו אפליקציה (שם כלשהו, למשל Callora). יופיעו <b dir="ltr">api_id</b> ו-<b dir="ltr">api_hash</b>.
        </li>
        <li>
          ב-GitHub, בהגדרות הריפו ← Secrets, הוסיפו <b dir="ltr">TELEGRAM_API_ID</b> ו-<b dir="ltr">TELEGRAM_API_HASH</b>.
        </li>
        <li>אחרי הפריסה הבאה יופיע כאן כפתור ״חבר חשבון טלגרם״.</li>
      </ol>
    </Card>
  );
}

function AccountCard({ account: a, onChange }: { account: TgAccount; onChange: () => Promise<void> }) {
  const status = STATUS[a.status] ?? { label: a.status, tone: "neutral" as const };
  const [confirmDelete, setConfirmDelete] = useState(false);
  const remove = async () => {
    await api(`/api/telegram/accounts/${a.id}`, { method: "DELETE" });
    await onChange();
  };
  const restart = async () => {
    await api(`/api/telegram/accounts/${a.id}/restart`, { method: "POST" });
    await onChange();
  };

  return (
    <section className="rounded-xl border border-slate-200 bg-white shadow-sm dark:border-slate-800 dark:bg-slate-900">
      <header className="flex flex-wrap items-center gap-3 border-b border-slate-100 px-5 py-4 dark:border-slate-800">
        <div className="flex size-10 items-center justify-center rounded-full bg-sky-500 text-white">
          <Send className="size-4" aria-hidden />
        </div>
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h2 className="font-semibold">{a.name}</h2>
            <Badge tone={status.tone}>{status.label}</Badge>
          </div>
          <div className="text-sm text-slate-500 dark:text-slate-400">
            {a.me ? (
              <>
                <span className="ltr tabular-nums">{tgWho(a.me)}</span>
                {a.me.name && ` · ${a.me.name}`}
              </>
            ) : (
              "לא מחובר"
            )}
          </div>
        </div>
        <div className="ms-auto flex items-center gap-2">
          {a.status === "ready" && isBroadcast(a) && <QueueSummary account={a} />}
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
        {a.status === "ready" ? (
          <>
            <Sending account={a} onChange={onChange} />
            {isBroadcast(a) ? (
              <Broadcast account={a} onChange={onChange} />
            ) : (
              <Problem>התפוצה לא נטענה (אין מסד נתונים בשרת).</Problem>
            )}
            <p className="mt-5 border-t border-slate-100 pt-4 text-sm text-slate-600 dark:border-slate-800 dark:text-slate-300">
              בוט המחירון נבחר ב
              <Link to="/settings" className="font-medium text-brand-700 underline-offset-2 hover:underline dark:text-brand-300">
                הגדרות ← בוט מחירים
              </Link>
              .
            </p>
          </>
        ) : (
          <Connect account={a} onRestart={restart} />
        )}
      </div>
    </section>
  );
}

/** Where orders go and who sends them: a group of its own (members only read), and a bot that
 * may send instead of the account. */
function Sending({ account: a, onChange }: { account: TgAccount; onChange: () => Promise<void> }) {
  const toast = useToast();
  const [groupName, setGroupName] = useState("נסיעות");
  const [making, setMaking] = useState(false);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const groups = a.groups ?? [];

  const makeGroup = async () => {
    setMaking(true);
    try {
      await api(`/api/telegram/accounts/${a.id}/group`, { method: "POST", body: JSON.stringify({ name: groupName.trim() || "נסיעות" }) });
      toast("הקבוצה נוצרה, וכל נסיעה תישלח אליה. שלחו את קישור ההצטרפות למי שצריך לראות.", "good");
      await onChange();
    } catch (e) {
      if (!(e instanceof Unauthorized)) toast("הקבוצה לא נוצרה. נסו שוב.", "bad");
    } finally {
      setMaking(false);
    }
  };
  const saveBot = async (value: string) => {
    setBusy(true);
    try {
      await api(`/api/telegram/accounts/${a.id}/bot`, { method: "POST", body: JSON.stringify({ token: value }) });
      setToken("");
      toast(value ? "הבוט חובר, ונוסף כמנהל לקבוצות הנסיעות." : "הבוט הוסר.", "good");
      await onChange();
    } catch (e) {
      if (!(e instanceof Unauthorized)) toast(e instanceof Error && e.message === "403" ? "הטוקן לא תקין. העתיקו אותו שוב מ-@BotFather." : "לא נשמר. נסו שוב.", "bad");
    } finally {
      setBusy(false);
    }
  };
  const setSender = async (viaBot: boolean) => {
    try {
      await api(`/api/telegram/accounts/${a.id}/sender`, { method: "POST", body: JSON.stringify({ via_bot: viaBot }) });
      await onChange();
    } catch (e) {
      if (!(e instanceof Unauthorized)) toast("לא נשמר. נסו שוב.", "bad");
    }
  };
  const copy = (link: string) =>
    navigator.clipboard.writeText(link).then(
      () => toast("הקישור הועתק", "good"),
      () => toast(link, "neutral"),
    );

  return (
    <div className="mb-6 grid gap-6 border-b border-slate-100 pb-6 dark:border-slate-800">
      <div>
        <h3 className="mb-1 flex items-center gap-2 font-semibold">
          <Users className="size-4 text-sky-600" aria-hidden />
          קבוצת נסיעות
        </h3>
        <p className="mb-3 text-sm text-slate-500 dark:text-slate-400">קבוצה חדשה שרק המערכת כותבת בה. כל נסיעה שהוזמנה נשלחת אליה, ומי שמצטרף רק קורא.</p>
        {groups.length > 0 && (
          <ul className="mb-3 grid gap-2">
            {groups.map((g) => (
              <li key={g.id} className="flex flex-wrap items-center gap-3 rounded-xl bg-slate-50 px-3.5 py-2.5 text-sm dark:bg-white/[0.04]">
                <span className="font-medium">{g.name}</span>
                <a href={g.link} target="_blank" rel="noreferrer" className="ltr min-w-0 truncate text-brand-700 hover:underline dark:text-brand-300">
                  {g.link}
                </a>
                <Button variant="ghost" onClick={() => copy(g.link)} title="העתק קישור הצטרפות" className="ms-auto">
                  <Copy className="size-4" aria-hidden />
                </Button>
              </li>
            ))}
          </ul>
        )}
        <div className="flex flex-wrap items-center gap-3">
          <Input aria-label="שם הקבוצה" value={groupName} onChange={(e) => setGroupName(e.target.value)} className="max-w-56" />
          <Button variant="primary" onClick={makeGroup} disabled={making}>
            <Plus className="size-4" aria-hidden />
            {making ? "יוצר…" : "צור קבוצה חדשה"}
          </Button>
        </div>
      </div>

      <div>
        <h3 className="mb-1 flex items-center gap-2 font-semibold">
          <Bot className="size-4 text-sky-600" aria-hidden />
          בוט שליחה
        </h3>
        {a.bot ? (
          <div className="grid gap-3">
            <div className="flex flex-wrap items-center gap-3 text-sm">
              <span>
                מחובר: <b className="ltr">@{a.bot.username}</b>
              </span>
              <Button variant="ghost" onClick={() => saveBot("")} disabled={busy}>
                הסר בוט
              </Button>
            </div>
            <Segmented
              label="מי שולח את הנסיעות"
              value={a.via_bot ? "bot" : "account"}
              options={[
                { value: "account", label: "החשבון" },
                { value: "bot", label: `הבוט @${a.bot.username}` },
              ]}
              onChange={(v) => void setSender(v === "bot")}
            />
            {a.via_bot && <p className="text-xs text-slate-500 dark:text-slate-400">בוט שולח רק לקבוצות שהוא מנהל בהן. לקבוצות שנוצרו כאן הוא נוסף לבד.</p>}
          </div>
        ) : (
          <>
            <p className="mb-3 text-sm text-slate-500 dark:text-slate-400">
              אפשר שהנסיעות יישלחו מבוט במקום מהחשבון. צרו בוט ב-<span dir="ltr">@BotFather</span>, והדביקו כאן את הטוקן שלו.
            </p>
            <div className="flex flex-wrap items-center gap-3">
              <Input
                type="password"
                autoComplete="off"
                dir="ltr"
                aria-label="הטוקן של הבוט"
                placeholder="123456789:AA…"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && token.trim() && void saveBot(token.trim())}
                className="max-w-80"
              />
              <Button onClick={() => saveBot(token.trim())} disabled={busy || !token.trim()}>
                {busy ? "בודק…" : "חבר בוט"}
              </Button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

type Sign = { status: TgAccount["status"]; qr: string | null; hint: string | null; error: string | null };

/** The QR code to scan, a new one every half minute; then the two-step password, if asked. */
function Connect({ account: a, onRestart }: { account: TgAccount; onRestart: () => Promise<void> }) {
  const [sign, setSign] = useState<Sign | null>(null);
  useEffect(() => {
    let alive = true;
    const load = () =>
      api<Sign>(`/api/telegram/accounts/${a.id}/qr`)
        .then((r) => alive && setSign(r))
        .catch(() => undefined);
    void load();
    const t = window.setInterval(load, 2000);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, [a.id]);

  const status = sign?.status ?? a.status;
  if (status === "password") return <Password account={a} hint={sign?.hint ?? null} wrong={sign?.error === "wrong_password"} />;

  return (
    <div className="flex flex-wrap items-center gap-8">
      <div className="flex size-64 items-center justify-center rounded-xl border border-slate-200 bg-white p-3 dark:border-slate-700">
        {sign?.qr ? (
          <img src={sign.qr} alt="קוד QR לחיבור טלגרם" className="size-full" />
        ) : (
          <Loading label={status === "failed" ? "לא עלה" : "מכין קוד…"} />
        )}
      </div>
      <div className="max-w-sm text-sm leading-relaxed text-slate-600 dark:text-slate-300">
        <h3 className="mb-2 font-semibold text-slate-900 dark:text-white">חיבור החשבון</h3>
        <ol className="list-decimal space-y-1 ps-5">
          <li>פתחו את טלגרם בטלפון של החשבון ששואל את הבוט.</li>
          <li>הגדרות ← מכשירים ← קישור מכשיר שולחני.</li>
          <li>סרקו את הקוד. הוא מתחלף לבד כל חצי דקה.</li>
        </ol>
        <p className="mt-3 text-xs text-slate-500 dark:text-slate-400">אם לחשבון יש אימות דו-שלבי, אחרי הסריקה תתבקשו להקליד כאן את הסיסמה.</p>
        {status === "disconnected" && (
          <p className="mt-3 text-amber-700 dark:text-amber-400">
            {sign?.error === "telegram_ended_the_login" || a.error === "telegram_ended_the_login"
              ? "טלגרם ניתקה את החיבור (מהטלפון: הגדרות ← מכשירים, או טלגרם עצמה). לחצו ״חבר מחדש״ וסרקו שוב."
              : "החשבון התנתק. לחצו ״חבר מחדש״ וסרקו שוב."}
          </p>
        )}
        {status === "failed" && (
          <div className="mt-3 flex flex-wrap items-center gap-3">
            <span className="text-rose-600 dark:text-rose-400">{sign?.error ? `שגיאה: ${sign.error}` : "החיבור לא עלה."}</span>
            <Button variant="secondary" onClick={onRestart}>
              נסו שוב
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}

function Password({ account: a, hint, wrong }: { account: TgAccount; hint: string | null; wrong: boolean }) {
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    if (!password) return;
    setBusy(true);
    try {
      await api(`/api/telegram/accounts/${a.id}/password`, { method: "POST", body: JSON.stringify({ password }) });
      setPassword("");
    } catch {
      /* the next poll shows where it stands */
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="flex max-w-md flex-col gap-4">
      <div className="flex items-center gap-3">
        <div className="flex size-10 items-center justify-center rounded-full bg-sky-50 text-sky-600 dark:bg-sky-500/10 dark:text-sky-300">
          <KeyRound className="size-5" aria-hidden />
        </div>
        <div>
          <h3 className="font-semibold text-slate-900 dark:text-white">הקוד נסרק. לחשבון יש אימות דו-שלבי</h3>
          <p className="text-sm text-slate-500 dark:text-slate-400">הקלידו את הסיסמה של טלגרם (לא הקוד שנשלח ב-SMS).</p>
        </div>
      </div>
      <label className="grid gap-1.5 text-sm">
        סיסמה{hint && <span className="text-xs text-slate-500 dark:text-slate-400">רמז: {hint}</span>}
        <Input
          type="password"
          autoComplete="current-password"
          autoFocus
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && void submit()}
        />
      </label>
      {wrong && <Problem>הסיסמה לא נכונה. נסו שוב.</Problem>}
      <div>
        <Button variant="primary" disabled={busy || !password} onClick={submit}>
          {busy ? "מתחבר…" : "התחברות"}
        </Button>
      </div>
    </div>
  );
}
