import { useState } from "react";
import { LoaderCircle, PhoneCall } from "lucide-react";
import { type CallModeData, Unauthorized } from "../api";
import { Badge, Button, Card, Field, Input, Problem, cx, useToast } from "../ui";

type Who = "callora" | "elevenlabs";

const OPTIONS: { value: Who; title: string; text: string }[] = [
  {
    value: "callora",
    title: "הסוכן של קלורה",
    text: "המנוע, הכללים, בדיקת הכתובות, הקריאה החוזרת והשליחה לוואטסאפ וטלגרם. המודל והקול נבחרים בכרטיסים שמתחת.",
  },
  {
    value: "elevenlabs",
    title: "סוכן של ElevenLabs",
    text: "השיחה עוברת לסוכן שבניתם בפלטפורמת ElevenLabs, עם ההגדרות שלו שם: ההנחיות, המודל, הקול והכלים. ElevenLabs מחייבת על זה.",
  },
];

/** Who answers the phone. The choice is used by the calls that come next; if ElevenLabs does not
 * answer a call, Callora's own agent takes it. */
export function CallModeCard({ mode, canSave, onChanged }: { mode: CallModeData; canSave: boolean; onChanged: () => void }) {
  const toast = useToast();
  const active: Who = mode.elevenlabs ? "elevenlabs" : "callora";
  const [who, setWho] = useState<Who>(active);
  const [agentId, setAgentId] = useState(mode.agent_id);
  const [saving, setSaving] = useState(false);
  const [problems, setProblems] = useState<string[]>([]);

  const unchanged = who === active && (who === "callora" || agentId.trim() === mode.agent_id);
  const blocked = who === "elevenlabs" && (!mode.available || !agentId.trim());

  const save = async () => {
    setSaving(true);
    setProblems([]);
    try {
      const res = await fetch("/api/settings/call-mode", {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ elevenlabs: who === "elevenlabs", agent_id: who === "elevenlabs" ? agentId.trim() : "" }),
      });
      if (res.status === 401) {
        window.dispatchEvent(new Event("callora:unauthorized"));
        throw new Unauthorized("unauthorized");
      }
      if (res.ok) {
        toast(who === "elevenlabs" ? "השיחות הבאות עוברות לסוכן של ElevenLabs." : "השיחות הבאות נענות בסוכן של קלורה.", "good");
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

  return (
    <Card
      title={
        <span className="inline-flex items-center gap-2">
          <PhoneCall className="size-4 text-brand-600 dark:text-brand-300" aria-hidden />
          מי עונה לטלפון
        </span>
      }
      subtitle={active === "elevenlabs" ? "עכשיו: סוכן של ElevenLabs" : "עכשיו: הסוכן של קלורה"}
      action={
        <Badge tone={mode.elevenlabs ? "brand" : "neutral"} dot>
          {mode.elevenlabs ? "ElevenLabs" : "קלורה"}
        </Badge>
      }
    >
      <div className="grid gap-5">
        <div role="radiogroup" aria-label="מי עונה לטלפון" className="grid gap-3 sm:grid-cols-2">
          {OPTIONS.map((o) => {
            const on = who === o.value;
            const off = o.value === "elevenlabs" && !mode.available;
            return (
              <button
                key={o.value}
                type="button"
                role="radio"
                aria-checked={on}
                disabled={off}
                onClick={() => {
                  setProblems([]);
                  setWho(o.value);
                }}
                className={cx(
                  "grid content-start gap-1 rounded-2xl p-4 text-start ring-1 ring-inset transition disabled:cursor-not-allowed disabled:opacity-50",
                  on
                    ? "bg-brand-50/80 ring-2 ring-brand-500 dark:bg-brand-400/10"
                    : "bg-white ring-slate-200 hover:ring-brand-300 dark:bg-white/[0.03] dark:ring-white/10",
                )}
              >
                <span className="text-sm font-semibold text-slate-900 dark:text-white">{o.title}</span>
                <span className="text-xs leading-relaxed text-slate-600 dark:text-slate-300">{o.text}</span>
                {off && <span className="text-xs text-rose-600 dark:text-rose-300">אין בשרת מפתח של ElevenLabs</span>}
              </button>
            );
          })}
        </div>

        {who === "elevenlabs" && (
          <div className="grid gap-3">
            <Field label="מזהה הסוכן ב-ElevenLabs" hint="מופיע בלוח של ElevenLabs, בפרטי הסוכן (agent_…). הוא נבדק מול ElevenLabs לפני השמירה.">
              {(id) => (
                <Input
                  id={id}
                  dir="ltr"
                  value={agentId}
                  onChange={(e) => {
                    setProblems([]);
                    setAgentId(e.target.value);
                  }}
                  placeholder="agent_…"
                />
              )}
            </Field>
            <ul className="grid gap-1 text-xs leading-relaxed text-slate-600 dark:text-slate-300">
              <li>• ההנחיות, המודל, הקול והכלים של הסוכן מוגדרים בלוח של ElevenLabs, לא כאן.</li>
              <li>• שיחות שעוברות אליו לא מופיעות בדף השיחות כאן, ולא עוברות בכללי קלורה (קריאה חוזרת, בדיקת כתובות, שליחה לוואטסאפ).</li>
              <li>• אם ElevenLabs לא עונה לשיחה, הסוכן של קלורה עונה במקומו.</li>
            </ul>
          </div>
        )}

        {problems.length > 0 && <Problem>{problems.join(" · ")}</Problem>}

        <div className="flex flex-wrap items-center gap-3 border-t border-slate-100 pt-5 dark:border-white/[0.06]">
          <Button variant="primary" onClick={save} disabled={!canSave || saving || unchanged || blocked} className="min-w-32">
            {saving ? (
              <>
                <LoaderCircle className="size-4 animate-spin" aria-hidden />
                בודק…
              </>
            ) : (
              "שמירה"
            )}
          </Button>
          <span className="text-xs leading-relaxed text-slate-500 dark:text-slate-400">
            {canSave ? "חל על השיחות הבאות. שיחה שכבר מתנהלת לא משתנה." : "שמירה לא זמינה בשרת הזה (אין מסד נתונים)."}
          </span>
        </div>
      </div>
    </Card>
  );
}
