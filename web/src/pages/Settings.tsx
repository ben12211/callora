import { useState } from "react";
import { Headphones, PhoneForwarded, Plus, Trash2 } from "lucide-react";
import { type DeskSettings, type SettingsData, Unauthorized, useApi } from "../api";
import { Badge, Button, Card, Loading, PageHeader, Problem } from "../ui";

const MUSIC: Record<string, string> = {
  classical: "קלאסית",
  ambient: "רגועה",
  electronica: "אלקטרונית",
  guitars: "גיטרות",
  "soft-rock": "רוק רך",
  none: "בלי מוזיקה (שקט)",
};

export function Settings() {
  const settings = useApi<SettingsData>("/api/settings");
  return (
    <>
      <PageHeader title="הגדרות" subtitle="מה קורה כשהשיחה צריכה בן אדם" />
      {settings.error && <Problem>{settings.error}</Problem>}
      {!settings.data && !settings.error ? (
        <Loading />
      ) : settings.data ? (
        <div className="grid max-w-3xl gap-6">
          {!settings.data.transfers && <Problem>PUBLIC_BASE_URL לא מוגדר בשרת, ולכן העברות למוקד לא יעבדו.</Problem>}
          {settings.data.businesses.map((b) => (
            <DeskCard key={b.id} id={b.id} name={b.name} desk={b.desk} music={settings.data!.music} canSave={settings.data!.saving} />
          ))}
        </div>
      ) : null}
    </>
  );
}

function DeskCard({ id, name, desk, music, canSave }: { id: string; name: string; desk: DeskSettings; music: string[]; canSave: boolean }) {
  const [numbers, setNumbers] = useState<string[]>(desk.numbers.length ? desk.numbers : [""]);
  const custom = !music.includes(desk.hold_music);
  const [musicChoice, setMusicChoice] = useState(custom ? "custom" : desk.hold_music);
  const [musicUrl, setMusicUrl] = useState(custom ? desk.hold_music : "");
  const [wait, setWait] = useState(desk.max_wait_seconds);
  const [status, setStatus] = useState<{ tone: "good" | "bad" | "neutral"; text: string } | null>(null);
  const [saving, setSaving] = useState(false);

  // A message about the last save goes away once something is edited again.
  const edited = <T,>(set: (v: T) => void) => (v: T) => {
    setStatus(null);
    set(v);
  };

  const save = async () => {
    setSaving(true);
    setStatus({ tone: "neutral", text: "שומר…" });
    const body = {
      numbers: numbers.map((n) => n.trim()).filter(Boolean),
      hold_music: musicChoice === "custom" ? musicUrl.trim() : musicChoice,
      max_wait_seconds: Number(wait),
    };
    try {
      const res = await fetch(`/api/settings/${encodeURIComponent(id)}`, {
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
        const saved = (await res.json()) as DeskSettings;
        setNumbers(saved.numbers.length ? saved.numbers : [""]);
        setStatus({ tone: "good", text: "נשמר. חל על השיחות הבאות." });
      } else if (res.status === 400) {
        const e = (await res.json()) as { problems?: string[] };
        setStatus({ tone: "bad", text: (e.problems ?? ["לא תקין"]).join(" · ") });
      } else {
        setStatus({ tone: "bad", text: `לא נשמר (${res.status})` });
      }
    } catch (e) {
      if (!(e instanceof Unauthorized)) setStatus({ tone: "bad", text: "אין חיבור לשרת" });
    } finally {
      setSaving(false);
    }
  };

  const field = "w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm dark:border-slate-700 dark:bg-slate-950";

  return (
    <Card
      title={
        <span className="inline-flex items-center gap-2">
          <PhoneForwarded className="size-4" aria-hidden />
          העברה למוקד שירות{name ? ` · ${name}` : ""}
        </span>
      }
      action={numbers.some((n) => n.trim()) ? <Badge tone="good">פעיל</Badge> : <Badge tone="warn">לא מוגדר</Badge>}
    >
      <div className="grid gap-6">
        <p className="text-sm text-slate-600 dark:text-slate-300">
          כשמתקשר מבקש נציג, או כשהסוכן לא מצליח לעזור, השיחה עוברת למוקד. כל המספרים מצלצלים יחד. הראשון שעונה שומע סיכום של השיחה ומקבל את המתקשר, והשאר מפסיקים לצלצל. בזמן ההמתנה המתקשר שומע מוזיקה.
        </p>

        <fieldset className="grid gap-2">
          <legend className="mb-1 text-sm font-medium">מספרי המוקד</legend>
          {numbers.map((n, i) => (
            <div key={i} className="flex gap-2">
              <input
                type="tel"
                dir="ltr"
                inputMode="tel"
                aria-label={`מספר ${i + 1}`}
                placeholder="050-1234567"
                value={n}
                onChange={(e) => edited(setNumbers)(numbers.map((x, j) => (j === i ? e.target.value : x)))}
                className={`${field} text-right tabular-nums`}
              />
              <Button variant="ghost" aria-label="הסרת המספר" onClick={() => edited(setNumbers)(numbers.length > 1 ? numbers.filter((_, j) => j !== i) : [""])}>
                <Trash2 className="size-4" aria-hidden />
              </Button>
            </div>
          ))}
          <div>
            <Button variant="ghost" onClick={() => edited(setNumbers)([...numbers, ""])} disabled={numbers.length >= 10}>
              <Plus className="size-4" aria-hidden />
              מספר נוסף
            </Button>
          </div>
          <p className="text-xs text-slate-500 dark:text-slate-400">אפשר לכתוב 050-1234567 או ‎+972501234567. בלי מספרים, המתקשר ישמע שאין מוקדן פנוי.</p>
        </fieldset>

        <div className="grid gap-2">
          <label htmlFor={`music-${id}`} className="inline-flex items-center gap-2 text-sm font-medium">
            <Headphones className="size-4" aria-hidden />
            מוזיקת המתנה
          </label>
          <select id={`music-${id}`} value={musicChoice} onChange={(e) => edited(setMusicChoice)(e.target.value)} className={field}>
            {music.map((m) => (
              <option key={m} value={m}>
                {MUSIC[m] ?? m}
              </option>
            ))}
            <option value="custom">קובץ משלי (קישור https)</option>
          </select>
          {musicChoice === "custom" && (
            <input
              type="url"
              dir="ltr"
              aria-label="קישור לקובץ המוזיקה"
              placeholder="https://…/music.mp3"
              value={musicUrl}
              onChange={(e) => edited(setMusicUrl)(e.target.value)}
              className={field}
            />
          )}
        </div>

        <div className="grid gap-2">
          <label htmlFor={`wait-${id}`} className="text-sm font-medium">
            כמה זמן לחכות למענה (שניות)
          </label>
          <input id={`wait-${id}`} type="number" min={15} max={600} value={wait} onChange={(e) => edited(setWait)(Number(e.target.value))} className={`${field} max-w-40`} />
          <p className="text-xs text-slate-500 dark:text-slate-400">אם אף אחד לא עונה עד אז, המתקשר שומע שאין מוקדן פנוי והשיחה מסתיימת.</p>
        </div>

        <div className="flex flex-wrap items-center gap-3 border-t border-slate-100 pt-4 dark:border-slate-800">
          <Button variant="primary" onClick={save} disabled={!canSave || saving}>
            שמירה
          </Button>
          {!canSave && <span className="text-sm text-slate-500">אין מסד נתונים: אי אפשר לשמור</span>}
          {status && (
            <span
              role="status"
              className={
                status.tone === "good"
                  ? "text-sm text-emerald-700 dark:text-emerald-400"
                  : status.tone === "bad"
                    ? "text-sm text-rose-700 dark:text-rose-400"
                    : "text-sm text-slate-500"
              }
            >
              {status.text}
            </span>
          )}
        </div>
      </div>
    </Card>
  );
}
