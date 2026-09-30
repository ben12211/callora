// The price-list bot the agent asks when a caller asks what a ride costs: which WhatsApp
// account asks it, which chat is the bot, and a test from here.

import { useEffect, useMemo, useState } from "react";
import { BadgeDollarSign, Search } from "lucide-react";
import { api, type PriceBotSettings, Unauthorized, useApi } from "../api";
import { phone } from "../format";
import { Badge, Button, Card, cx, Field, FIELD, Input, Problem, useToast } from "../ui";
import type { WaOverview } from "./WhatsApp";

type Chats = { groups: { id: string; name: string }[]; contacts: { id: string; name: string; number: string }[] };

type Quote = {
  price: number;
  price_6?: number;
  round_trip?: number;
  vehicle?: string;
  neighborhood?: boolean;
  tier?: string | null;
  question?: string;
  answer?: string;
  cached?: boolean;
};
type TestResult = { ok: boolean; ms: number; quote?: Quote & { more_than_one_car?: boolean }; error?: string };

/** What went wrong, in words the owner can act on. */
function problem(error?: string): string {
  const e = error ?? "";
  if (e.includes("no answer in time")) return "הבוט לא ענה בזמן (15 שניות). בדקו שהוא עונה למספר הזה מהטלפון.";
  if (e.includes("not connected")) return "חשבון הוואטסאפ לא מחובר. חברו אותו מחדש בדף וואטסאפ.";
  if (e.includes("refused")) return "הבוט לא שמור כאיש קשר בחשבון הזה. שמרו אותו בטלפון ונסו שוב.";
  if (e.includes("no prices")) return "הבוט ענה, אבל בלי מחירים. אולי הניסוח של השאלה לא מתאים לו.";
  if (e.includes("no backend")) return "לא נבחר בוט מחירים.";
  if (e.includes("route is not known")) return "חסר מאיפה או לאן.";
  return e || "הבוט לא ענה";
}

export function PriceBotCard({ id, bot, whatsapp, canSave }: { id: string; bot: PriceBotSettings | null; whatsapp: boolean; canSave: boolean }) {
  const toast = useToast();
  const overview = useApi<WaOverview>(whatsapp ? "/api/whatsapp" : null);
  const accounts = useMemo(() => (overview.data?.accounts ?? []).filter((a) => a.status === "ready"), [overview.data]);
  const [account, setAccount] = useState(bot?.account ?? "");
  const [chat, setChat] = useState<{ id: string; name: string } | null>(bot ? { id: bot.chat_id, name: bot.chat_name } : null);
  const [query, setQuery] = useState("");
  const [saved, setSaved] = useState<PriceBotSettings | null>(bot);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!account && accounts.length === 1) setAccount(accounts[0].id);
  }, [account, accounts]);

  const chats = useApi<Chats>(account ? `/api/whatsapp/accounts/${account}/chats` : null);
  const contacts = useMemo(() => {
    const q = query.trim();
    return (chats.data?.contacts ?? []).filter((c) => !q || c.name.includes(q) || c.number.includes(q.replace(/\D/g, "") || "~"));
  }, [chats.data, query]);

  const save = async (value: PriceBotSettings | null) => {
    setSaving(true);
    try {
      const result = await api<PriceBotSettings | null>(`/api/settings/${encodeURIComponent(id)}/price-bot`, {
        method: "PUT",
        body: JSON.stringify(value),
      });
      setSaved(result);
      if (!result) setChat(null);
      toast(result ? "בוט המחירים נשמר. שאלות מחיר בשיחות ילכו אליו." : "שאלות מחיר לא יישאלו יותר.", "good");
    } catch (e) {
      if (!(e instanceof Unauthorized)) toast("לא נשמר", "bad");
    } finally {
      setSaving(false);
    }
  };

  const changed = !!chat && (saved?.account !== account || saved?.chat_id !== chat.id);

  return (
    <Card
      title={
        <span className="inline-flex items-center gap-2">
          <BadgeDollarSign className="size-4 text-brand-600 dark:text-brand-300" aria-hidden />
          בוט מחירים
        </span>
      }
      subtitle="כשמתקשר שואל כמה עולה נסיעה, הסוכן שואל את הבוט בוואטסאפ ואומר את המחיר"
      action={saved ? <Badge tone="good" dot>פעיל</Badge> : <Badge tone="warn" dot>לא מוגדר</Badge>}
    >
      {!whatsapp ? (
        <Problem>שירות הוואטסאפ לא מוגדר בשרת.</Problem>
      ) : (
        <div className="grid gap-6">
          <p className="text-sm leading-relaxed text-slate-600 dark:text-slate-300">
            שמרו את הבוט כאיש קשר בטלפון של החשבון ששואל, ושלחו לו הודעה אחת מהטלפון כדי לפתוח את הצ'אט. אחר כך בחרו אותו כאן.
          </p>

          <Field label="החשבון ששואל" hint={accounts.length ? undefined : "אין חשבון וואטסאפ מחובר. חברו אחד בדף וואטסאפ."}>
            {(fid) => (
              <select
                id={fid}
                value={account}
                onChange={(e) => {
                  setAccount(e.target.value);
                  setChat(null);
                }}
                className={cx(FIELD, "cursor-pointer")}
              >
                <option value="">בחרו חשבון</option>
                {accounts.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name}
                    {a.me ? ` · ${phone(`+${a.me.number}`)}` : ""}
                  </option>
                ))}
              </select>
            )}
          </Field>

          {account && (
            <div className="grid gap-2">
              <div className="text-sm font-medium">הבוט</div>
              {chat && (
                <div className="flex items-center justify-between rounded-xl bg-brand-50/70 px-3.5 py-2.5 text-sm ring-1 ring-inset ring-brand-100 dark:bg-brand-400/[0.07] dark:ring-brand-400/15">
                  <span className="font-medium">{chat.name || chat.id}</span>
                  <Button variant="ghost" className="px-2 py-1 text-xs" onClick={() => setChat(null)}>
                    החלפה
                  </Button>
                </div>
              )}
              {!chat && (
                <>
                  <div className="relative">
                    <Search className="pointer-events-none absolute start-3 top-1/2 size-4 -translate-y-1/2 text-slate-400" aria-hidden />
                    <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="חיפוש איש קשר" aria-label="חיפוש איש קשר" className="ps-9" />
                  </div>
                  <ul className="max-h-60 divide-y divide-slate-100 overflow-y-auto rounded-xl ring-1 ring-inset ring-slate-200 dark:divide-white/[0.06] dark:ring-white/10">
                    {chats.loading && <li className="px-3.5 py-3 text-sm text-slate-500">טוען אנשי קשר…</li>}
                    {!chats.loading && contacts.length === 0 && <li className="px-3.5 py-3 text-sm text-slate-500">לא נמצא איש קשר</li>}
                    {contacts.slice(0, 50).map((c) => (
                      <li key={c.id}>
                        <button
                          type="button"
                          onClick={() => setChat({ id: c.id, name: c.name })}
                          className="flex w-full items-center justify-between px-3.5 py-2.5 text-start text-sm hover:bg-slate-50 dark:hover:bg-white/[0.04]"
                        >
                          <span>{c.name}</span>
                          <span className="num text-xs text-slate-500" dir="ltr">
                            {phone(`+${c.number}`)}
                          </span>
                        </button>
                      </li>
                    ))}
                  </ul>
                </>
              )}
            </div>
          )}

          <div className="flex flex-wrap items-center gap-3 border-t border-slate-100 pt-5 dark:border-white/[0.06]">
            <Button
              variant="primary"
              disabled={!canSave || saving || !changed}
              onClick={() => chat && save({ account, chat_id: chat.id, chat_name: chat.name })}
              className="min-w-28"
            >
              {saving ? "שומר…" : "שמירה"}
            </Button>
            {saved && (
              <Button variant="ghost" disabled={saving} onClick={() => save(null)}>
                הפסקת שאלות מחיר
              </Button>
            )}
          </div>

          {saved && <PriceTest id={id} />}
        </div>
      )}
    </Card>
  );
}

function PriceTest({ id }: { id: string }) {
  const [from, setFrom] = useState("בני ברק");
  const [to, setTo] = useState("ירושלים");
  const [people, setPeople] = useState("");
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<TestResult | null>(null);

  const run = async () => {
    setBusy(true);
    setResult(null);
    try {
      const body = { from: from.trim(), to: to.trim(), passengers: people ? Number(people) : undefined };
      setResult(await api<TestResult>(`/api/settings/${encodeURIComponent(id)}/price-bot/test`, { method: "POST", body: JSON.stringify(body) }));
    } catch (e) {
      if (!(e instanceof Unauthorized)) setResult({ ok: false, ms: 0, error: "אין חיבור לשרת" });
    } finally {
      setBusy(false);
    }
  };

  const q = result?.quote;
  return (
    <div className="grid gap-4 rounded-2xl bg-slate-50 p-4 ring-1 ring-inset ring-slate-200/70 dark:bg-white/[0.03] dark:ring-white/[0.06]">
      <div className="text-sm font-semibold">בדיקה</div>
      <div className="grid gap-3 sm:grid-cols-[1fr_1fr_7rem_auto] sm:items-end">
        <label className="grid gap-1.5 text-sm">
          מאיפה
          <Input value={from} onChange={(e) => setFrom(e.target.value)} />
        </label>
        <label className="grid gap-1.5 text-sm">
          לאן
          <Input value={to} onChange={(e) => setTo(e.target.value)} />
        </label>
        <label className="grid gap-1.5 text-sm">
          נוסעים
          <Input type="number" min={1} max={20} value={people} onChange={(e) => setPeople(e.target.value)} placeholder="לא ידוע" className="num" />
        </label>
        <Button variant="secondary" disabled={busy || !from.trim() || !to.trim()} onClick={run}>
          {busy ? "שואל את הבוט…" : "כמה זה עולה?"}
        </Button>
      </div>

      {result && !result.ok && <Problem>{problem(result.error)}</Problem>}
      {q && (
        <div className="grid gap-3 text-sm">
          <div className="flex flex-wrap items-baseline gap-x-5 gap-y-1">
            <span>
              {q.more_than_one_car ? "הרכב הכי גדול" : people ? "המחיר" : "עד 4 נוסעים"}: <b className="num text-lg">₪{q.price}</b>
            </span>
            {q.more_than_one_car && <Badge tone="warn">צריך יותר מרכב אחד</Badge>}
            {q.price_6 != null && (
              <span>
                עד 6: <b className="num text-lg">₪{q.price_6}</b>
              </span>
            )}
            {q.round_trip != null && (
              <span>
                הלוך חזור: <b className="num">₪{q.round_trip}</b>
              </span>
            )}
            {q.vehicle && <Badge>{q.vehicle}</Badge>}
            {q.neighborhood && <Badge tone="warn">תוספת שכונה</Badge>}
            {q.tier && <Badge tone="brand">מחירון {q.tier}</Badge>}
          </div>
          <div className="text-xs text-slate-500 dark:text-slate-400">
            נשאל: <span className="font-medium">{q.question}</span> · <span className="num">{(result!.ms / 1000).toFixed(1)}</span> שניות
            {q.cached ? " · מהזיכרון (נשאל בחצי השעה האחרונה)" : ""}
          </div>
          {q.answer && (
            <details className="rounded-xl bg-white p-3 ring-1 ring-inset ring-slate-200/70 dark:bg-slate-900/60 dark:ring-white/[0.06]">
              <summary className="cursor-pointer text-xs font-medium text-slate-600 dark:text-slate-300">התשובה של הבוט</summary>
              <pre className="mt-2 whitespace-pre-wrap font-sans text-xs leading-relaxed text-slate-700 dark:text-slate-300">{q.answer}</pre>
            </details>
          )}
        </div>
      )}
    </div>
  );
}
