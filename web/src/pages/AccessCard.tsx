import { useEffect, useState } from "react";
import { FlaskConical, Plus, Radio, Trash2 } from "lucide-react";
import { Unauthorized, useApi } from "../api";
import { phone } from "../format";
import { Badge, Button, Card, Input, Problem, cx, useToast } from "../ui";

type Mode = "development" | "production";
type Access = { mode: Mode; numbers: string[]; from_deployment: boolean; saving: boolean };

/** Who may call: in development only the numbers on the list (the owner's and testers'), in
 * production anyone. A blocked number never, in either. */
export function AccessCard() {
  const toast = useToast();
  const access = useApi<Access>("/api/access");
  const [mode, setMode] = useState<Mode>("development");
  const [numbers, setNumbers] = useState<string[]>([]);
  const [adding, setAdding] = useState("");
  const [problems, setProblems] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (access.data) {
      setMode(access.data.mode);
      setNumbers(access.data.numbers);
    }
  }, [access.data]);

  if (access.error) return <Problem>{access.error}</Problem>;
  if (!access.data) return null;
  const a = access.data;
  const dirty = mode !== a.mode || JSON.stringify(numbers) !== JSON.stringify(a.numbers);

  const add = () => {
    const n = adding.trim();
    if (!n) return;
    setNumbers([...numbers, n]);
    setAdding("");
    setProblems([]);
  };

  const save = async () => {
    setSaving(true);
    setProblems([]);
    try {
      const res = await fetch("/api/access", {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ mode, numbers }),
      });
      if (res.status === 401) {
        window.dispatchEvent(new Event("callora:unauthorized"));
        throw new Unauthorized("unauthorized");
      }
      if (res.ok) {
        toast(mode === "production" ? "המערכת פעילה: כל אחד יכול להתקשר." : "מצב פיתוח: רק המספרים ברשימה יכולים להתקשר.", "good");
        await access.reload();
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

  const options: { value: Mode; title: string; text: string; icon: typeof Radio }[] = [
    { value: "development", title: "פיתוח", text: "רק המספרים ברשימה יכולים להתקשר. כל השאר שומעים שהמספר לא זמין.", icon: FlaskConical },
    { value: "production", title: "פעיל", text: "כל אחד יכול להתקשר ולהזמין מונית.", icon: Radio },
  ];

  return (
    <Card
      title="מצב המערכת"
      subtitle="מי יכול להתקשר לקו. מספר חסום (מדף השיחה) לא יכול להתקשר בשום מצב."
      action={a.mode === "production" ? <Badge tone="good" dot>פעיל לכולם</Badge> : <Badge tone="warn" dot>פיתוח</Badge>}
    >
      <div className="grid gap-6">
        <fieldset className="grid gap-2.5 sm:grid-cols-2">
          <legend className="sr-only">מצב</legend>
          {options.map(({ value, title, text, icon: Icon }) => (
            <label
              key={value}
              className={cx(
                "flex cursor-pointer gap-3 rounded-2xl p-4 ring-1 ring-inset transition",
                mode === value ? "bg-brand-50/70 ring-brand-300 dark:bg-brand-400/[0.08] dark:ring-brand-400/40" : "ring-slate-200 hover:bg-slate-50 dark:ring-white/10 dark:hover:bg-white/[0.03]",
              )}
            >
              <input type="radio" name="access-mode" value={value} checked={mode === value} onChange={() => setMode(value)} className="mt-1 size-4 accent-brand-600" />
              <div>
                <div className="flex items-center gap-2 font-semibold text-slate-900 dark:text-white">
                  <Icon className="size-4 text-brand-600 dark:text-brand-300" aria-hidden />
                  {title}
                </div>
                <div className="mt-0.5 text-xs leading-relaxed text-slate-500 dark:text-slate-400">{text}</div>
              </div>
            </label>
          ))}
        </fieldset>

        <div className="grid gap-2.5">
          <div className="text-sm font-medium">
            רשימת הגישה <span className="text-slate-500">({numbers.length})</span>
          </div>
          <p className="text-xs text-slate-500 dark:text-slate-400">{mode === "production" ? "נשמרת לפעם הבאה שתעבור למצב פיתוח." : "רק המספרים האלה יכולים להתקשר עכשיו."}</p>
          {numbers.length > 0 && (
            <ul className="divide-y divide-slate-100 rounded-xl ring-1 ring-inset ring-slate-200 dark:divide-white/[0.06] dark:ring-white/10">
              {numbers.map((n, i) => (
                <li key={`${n}-${i}`} className="flex items-center gap-3 px-3.5 py-2">
                  <span className="ltr num flex-1 text-start text-sm">{n.startsWith("+") ? phone(n) : n}</span>
                  <Button variant="ghost" className="px-2" aria-label={`הסרת ${n}`} onClick={() => setNumbers(numbers.filter((_, j) => j !== i))}>
                    <Trash2 className="size-4" aria-hidden />
                  </Button>
                </li>
              ))}
            </ul>
          )}
          <div className="flex gap-2">
            <Input
              type="tel"
              dir="ltr"
              inputMode="tel"
              placeholder="050-1234567"
              aria-label="מספר להוספה"
              value={adding}
              onChange={(e) => setAdding(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && add()}
              className="num max-w-56 text-right"
            />
            <Button onClick={add} disabled={!adding.trim()}>
              <Plus className="size-4" aria-hidden />
              הוספה
            </Button>
          </div>
        </div>

        {problems.length > 0 && <Problem>{problems.join(" · ")}</Problem>}
        {mode === "production" && a.mode === "development" && <Problem>אחרי השמירה כל אחד יוכל להתקשר לקו ולהזמין מונית.</Problem>}

        <div className="flex flex-wrap items-center gap-3 border-t border-slate-100 pt-5 dark:border-white/[0.06]">
          <Button variant="primary" onClick={save} disabled={!a.saving || saving || !dirty} className="min-w-28">
            {saving ? "שומר…" : "שמירה"}
          </Button>
          {dirty && (
            <Button
              variant="ghost"
              onClick={() => {
                setMode(a.mode);
                setNumbers(a.numbers);
                setProblems([]);
              }}
            >
              ביטול
            </Button>
          )}
          {a.from_deployment && <span className="text-xs text-slate-500">עכשיו לפי הגדרות השרת (ALLOW_LIST). אחרי שמירה ראשונה, לפי מה שכאן.</span>}
        </div>
      </div>
    </Card>
  );
}
