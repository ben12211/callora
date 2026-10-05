import { useState } from "react";
import { Brain, LoaderCircle } from "lucide-react";
import { type AgentModelChoice, type AgentModelData, Unauthorized } from "../api";
import { Badge, Button, Card, Field, FIELD, Input, Problem, cx, useToast } from "../ui";

const CUSTOM = "__custom";
const NO_BACKUP = "none";

const EFFORTS: { value: string; label: string }[] = [
  { value: "", label: "ברירת המחדל של הספק" },
  { value: "none", label: "ללא חשיבה (הכי מהיר)" },
  { value: "low", label: "נמוכה" },
  { value: "medium", label: "בינונית" },
  { value: "high", label: "גבוהה (הכי איטי)" },
];

type Models = AgentModelData["models"];

/** A model already on the list, or "other" with its name typed. */
function pick(model: string, models: Models, extra: string[] = []): [string, string] {
  return models.some((m) => m.id === model) || extra.includes(model) ? [model, ""] : [CUSTOM, model];
}

/** Which model runs the agent, and which backs it up. A new choice is tried with a small
 * request before it is used; calls pick it up from their next turn. */
export function AgentModelCard({ agent, canSave, onChanged }: { agent: AgentModelData; canSave: boolean; onChanged: () => void }) {
  const toast = useToast();
  const [primary, primaryTyped] = pick(agent.active.primary, agent.models);
  const [backup, backupTyped] = pick(agent.active.backup, agent.models, [NO_BACKUP]);
  const [primaryChoice, setPrimaryChoice] = useState(primary);
  const [primaryName, setPrimaryName] = useState(primaryTyped);
  const [backupChoice, setBackupChoice] = useState(backup);
  const [backupName, setBackupName] = useState(backupTyped);
  const [effort, setEffort] = useState(agent.active.effort ?? "");
  const [saving, setSaving] = useState(false);
  const [problems, setProblems] = useState<string[]>([]);

  const wanted: AgentModelChoice = {
    primary: (primaryChoice === CUSTOM ? primaryName : primaryChoice).trim(),
    backup: (backupChoice === CUSTOM ? backupName : backupChoice).trim() || NO_BACKUP,
    effort: effort || null,
  };
  const unchanged =
    wanted.primary === agent.active.primary && wanted.backup === agent.active.backup && (wanted.effort ?? "") === (agent.active.effort ?? "");

  const send = async (body: AgentModelChoice | null) => {
    setSaving(true);
    setProblems([]);
    try {
      const res = await fetch("/api/settings/agent-model", {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      if (res.status === 401) {
        window.dispatchEvent(new Event("callora:unauthorized"));
        throw new Unauthorized("unauthorized");
      }
      if (res.ok) {
        toast("המודל הוחלף. הוא חל מהתור הבא בכל שיחה.", "good");
        onChanged();
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

  const edited = <T,>(set: (v: T) => void) => (v: T) => {
    setProblems([]);
    set(v);
  };

  const options = (withNone: boolean) => (
    <>
      {withNone && <option value={NO_BACKUP}>בלי גיבוי (המודל הראשי לבד)</option>}
      {agent.models.map((m) => {
        const noKey = !agent.providers[m.provider];
        return (
          <option key={m.id} value={m.id} disabled={noKey}>
            {m.id} · {m.provider === "gemini" ? "Gemini" : "OpenAI"}
            {noKey ? " (אין מפתח בשרת)" : ""}
          </option>
        );
      })}
      <option value={CUSTOM}>אחר… (להקליד שם מודל)</option>
    </>
  );

  return (
    <Card
      title={
        <span className="inline-flex items-center gap-2">
          <Brain className="size-4 text-brand-600 dark:text-brand-300" aria-hidden />
          המודל של הסוכן
        </span>
      }
      subtitle={`בשימוש עכשיו: ${agent.active.primary}${agent.active.backup !== NO_BACKUP ? `, בגיבוי ${agent.active.backup}` : ""}`}
      action={
        <Badge tone={agent.custom ? "brand" : "neutral"} dot>
          {agent.custom ? "נבחר כאן" : "ברירת מחדל"}
        </Badge>
      }
    >
      <div className="grid gap-5">
        <div className="grid gap-5 sm:grid-cols-2">
          <div className="grid content-start gap-2">
            <Field label="המודל הראשי" hint="המודל שמנהל את השיחה.">
              {(id) => (
                <select id={id} value={primaryChoice} onChange={(e) => edited(setPrimaryChoice)(e.target.value)} className={cx(FIELD, "ltr")}>
                  {options(false)}
                </select>
              )}
            </Field>
            {primaryChoice === CUSTOM && (
              <Input dir="ltr" value={primaryName} onChange={(e) => edited(setPrimaryName)(e.target.value)} placeholder="למשל gpt-6-sol" aria-label="שם המודל הראשי" />
            )}
          </div>
          <div className="grid content-start gap-2">
            <Field label="מודל הגיבוי" hint="אם הראשי עוד לא התחיל לענות אחרי 1.2 שניות, הגיבוי נשאל במקביל, והראשון שעונה מנצח.">
              {(id) => (
                <select id={id} value={backupChoice} onChange={(e) => edited(setBackupChoice)(e.target.value)} className={cx(FIELD, "ltr")}>
                  {options(true)}
                </select>
              )}
            </Field>
            {backupChoice === CUSTOM && (
              <Input dir="ltr" value={backupName} onChange={(e) => edited(setBackupName)(e.target.value)} placeholder="למשל gpt-6-luna" aria-label="שם מודל הגיבוי" />
            )}
          </div>
        </div>

        <Field label="כמה המודל חושב לפני המילה הראשונה" hint="כל מדרגה למעלה מאטה את התשובה בטלפון. ההגדרה חלה על מודלי חשיבה בלבד.">
          {(id) => (
            <select id={id} value={effort} onChange={(e) => edited(setEffort)(e.target.value)} className={cx(FIELD, "sm:max-w-xs")}>
              {EFFORTS.map((e) => (
                <option key={e.value} value={e.value}>
                  {e.label}
                </option>
              ))}
            </select>
          )}
        </Field>

        {problems.length > 0 && <Problem>{problems.join(" · ")}</Problem>}

        <div className="flex flex-wrap items-center gap-3 border-t border-slate-100 pt-5 dark:border-white/[0.06]">
          <Button variant="primary" onClick={() => send(wanted)} disabled={!canSave || saving || unchanged || !wanted.primary} className="min-w-32">
            {saving ? (
              <>
                <LoaderCircle className="size-4 animate-spin" aria-hidden />
                בודק…
              </>
            ) : (
              "החלפת המודל"
            )}
          </Button>
          {agent.custom && (
            <Button variant="ghost" onClick={() => send(null)} disabled={!canSave || saving}>
              חזרה לברירת המחדל ({agent.defaults.primary})
            </Button>
          )}
          <span className="text-xs leading-relaxed text-slate-500 dark:text-slate-400">
            {canSave
              ? "לפני ההחלפה כל מודל עונה על בקשה קצרה, ואם הוא לא עונה ההחלפה נעצרת. מומלץ להשוות קודם עם callora eval. עלות לשיחה תוצג רק למודל שיש לו מחיר ב-AGENT_PRICES."
              : "שמירה לא זמינה בשרת הזה (אין מסד נתונים)."}
          </span>
        </div>
      </div>
    </Card>
  );
}
