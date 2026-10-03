import { useState } from "react";
import { AudioLines, Check, LoaderCircle } from "lucide-react";
import { type VoiceSettings, Unauthorized } from "../api";
import { Badge, Button, Card, Problem, cx, useToast } from "../ui";

/** Which voice the agent speaks with. Choosing one records every sentence in it first (about a
 * minute); calls keep the voice they started with, so none hears two voices. */
export function VoiceCard({
  id,
  voice,
  canSave,
  onChanged,
}: {
  id: string;
  voice: VoiceSettings;
  canSave: boolean;
  onChanged: () => void;
}) {
  const toast = useToast();
  const [choice, setChoice] = useState(voice.chosen ?? voice.active ?? "");
  const [saving, setSaving] = useState(false);
  const [problems, setProblems] = useState<string[]>([]);

  const building = voice.building && !voice.building.error ? voice.building : null;
  const failed = voice.building?.error ? voice.building : null;
  const nameOf = (v: string | null | undefined) => voice.choices.find((c) => c.id === v)?.name ?? v ?? "—";

  const save = async () => {
    setSaving(true);
    setProblems([]);
    try {
      const res = await fetch(`/api/settings/${encodeURIComponent(id)}/voice`, {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ voice_id: choice }),
      });
      if (res.status === 401) {
        window.dispatchEvent(new Event("callora:unauthorized"));
        throw new Unauthorized("unauthorized");
      }
      if (res.ok) {
        toast(choice === voice.active ? "זה כבר הקול בשימוש." : "מקליטים את המשפטים בקול החדש. זה לוקח בערך דקה.", "good");
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
          <AudioLines className="size-4 text-brand-600 dark:text-brand-300" aria-hidden />
          הקול של הסוכן
        </span>
      }
      subtitle={`בשימוש עכשיו: ${nameOf(voice.active)}`}
      action={
        building ? (
          <Badge tone="warn" dot>
            מקליט את {nameOf(building.voice)}…
          </Badge>
        ) : (
          <Badge tone="good" dot>
            {nameOf(voice.active)}
          </Badge>
        )
      }
    >
      <div className="grid gap-5">
        <fieldset className="grid gap-2.5" disabled={!!building}>
          <legend className="sr-only">בחירת קול</legend>
          {voice.choices.map((c) => {
            const selected = choice === c.id;
            const active = voice.active === c.id;
            return (
              <label
                key={c.id}
                className={cx(
                  "flex cursor-pointer flex-wrap items-center gap-3 rounded-2xl p-3.5 ring-1 ring-inset transition",
                  selected
                    ? "bg-brand-50/70 ring-brand-300 dark:bg-brand-400/[0.08] dark:ring-brand-400/40"
                    : "ring-slate-200 hover:bg-slate-50 dark:ring-white/10 dark:hover:bg-white/[0.03]",
                )}
              >
                <input
                  type="radio"
                  name={`voice-${id}`}
                  value={c.id}
                  checked={selected}
                  onChange={() => {
                    setProblems([]);
                    setChoice(c.id);
                  }}
                  className="size-4 accent-brand-600"
                />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2 font-semibold text-slate-900 dark:text-white">
                    {c.name}
                    {active && (
                      <span className="inline-flex items-center gap-1 text-xs font-medium text-emerald-700 dark:text-emerald-300">
                        <Check className="size-3.5" aria-hidden />
                        בשימוש
                      </span>
                    )}
                  </div>
                  {c.description && <div className="text-xs text-slate-500 dark:text-slate-400">{c.description}</div>}
                </div>
                {c.preview && <audio controls preload="none" src={c.preview} className="h-9 w-full max-w-64" aria-label={`דוגמה של ${c.name}`} />}
              </label>
            );
          })}
        </fieldset>

        {failed && <Problem>ההחלפה ל{nameOf(failed.voice)} לא הצליחה, והקול הקודם נשאר: {failed.error}</Problem>}
        {problems.length > 0 && <Problem>{problems.join(" · ")}</Problem>}

        <div className="flex flex-wrap items-center gap-3 border-t border-slate-100 pt-5 dark:border-white/[0.06]">
          <Button variant="primary" onClick={save} disabled={!canSave || saving || !!building || !choice} className="min-w-32">
            {building ? (
              <>
                <LoaderCircle className="size-4 animate-spin" aria-hidden />
                מקליט…
              </>
            ) : saving ? (
              "שומר…"
            ) : (
              "החלפת הקול"
            )}
          </Button>
          <span className="text-xs leading-relaxed text-slate-500 dark:text-slate-400">
            {canSave
              ? "קול חדש מוקלט פעם אחת (כדקה, ועולה קרדיטים ב-ElevenLabs); חזרה לקול שכבר הוקלט מיידית. שיחות פעילות ממשיכות בקול שלהן."
              : "החלפת קול לא זמינה בשרת הזה."}
          </span>
        </div>
      </div>
    </Card>
  );
}
