import { useState } from "react";
import { Headphones, PhoneForwarded, PhoneIncoming, PhoneOutgoing, Plus, Trash2, UserRoundCheck } from "lucide-react";
import { type DeskSettings, type SettingsData, Unauthorized, useApi } from "../api";
import { Badge, Button, Card, Field, FIELD, Input, PageHeader, Problem, Skeleton, cx, useToast } from "../ui";
import { PriceBotCard } from "./PriceBot";
import { VoiceCard } from "./VoiceCard";

const MUSIC: Record<string, string> = {
  classical: "קלאסית",
  ambient: "רגועה",
  electronica: "אלקטרונית",
  guitars: "גיטרות",
  "soft-rock": "רוק רך",
  none: "בלי מוזיקה (שקט)",
};

const STEPS = [
  { icon: PhoneIncoming, title: "המתקשר מבקש נציג", text: "או שהסוכן לא מצליח לעזור." },
  { icon: Headphones, title: "הוא שומע מוזיקה", text: "וכל מספרי המוקד מצלצלים יחד." },
  { icon: UserRoundCheck, title: "הראשון שעונה מקבל אותו", text: "אחרי סיכום קצר של השיחה." },
];

export function Settings() {
  const [watching, setWatching] = useState(false);
  // While a voice is being recorded, the page follows it until it is in use.
  const settings = useApi<SettingsData>("/api/settings", watching ? 3000 : undefined);
  const building = settings.data?.businesses.some((b) => b.voice.building && !b.voice.building.error) ?? false;
  if (watching !== building && settings.data) setWatching(building);
  return (
    <>
      <PageHeader title="הגדרות" subtitle="הקול של הסוכן, העברה למוקד, ומאיפה הסוכן יודע מחירים" />
      {settings.error && <Problem>{settings.error}</Problem>}
      {!settings.data && !settings.error ? (
        <div className="grid max-w-3xl gap-6">
          <Skeleton className="h-28 rounded-2xl" />
          <Skeleton className="h-96 rounded-2xl" />
        </div>
      ) : settings.data ? (
        <div className="grid max-w-3xl gap-6">
          {!settings.data.transfers && <Problem>PUBLIC_BASE_URL לא מוגדר בשרת, ולכן העברות למוקד לא יעבדו.</Problem>}
          <ol className="grid gap-3 sm:grid-cols-3">
            {STEPS.map(({ icon: Icon, title, text }, i) => (
              <li key={title} className="flex gap-3 rounded-2xl bg-brand-50/70 p-4 ring-1 ring-inset ring-brand-100 dark:bg-brand-400/[0.07] dark:ring-brand-400/15">
                <span className="flex size-9 shrink-0 items-center justify-center rounded-xl bg-white text-brand-600 shadow-sm dark:bg-white/10 dark:text-brand-200">
                  <Icon className="size-[18px]" aria-hidden />
                </span>
                <div className="text-sm">
                  <div className="font-semibold text-slate-900 dark:text-white">
                    <span className="num me-1 text-brand-500">{i + 1}.</span>
                    {title}
                  </div>
                  <div className="mt-0.5 text-xs leading-relaxed text-slate-600 dark:text-slate-300">{text}</div>
                </div>
              </li>
            ))}
          </ol>
          {settings.data.businesses
            .filter((b) => b.voice.choices.length > 0)
            .map((b) => (
              <VoiceCard
                key={`voice-${b.id}`}
                id={b.id}
                voice={b.voice}
                canSave={settings.data!.saving && settings.data!.voice_switching}
                onChanged={() => {
                  setWatching(true);
                  settings.reload();
                }}
              />
            ))}
          {settings.data.businesses.map((b) => (
            <DeskCard key={b.id} id={b.id} name={b.name} desk={b.desk} music={settings.data!.music} canSave={settings.data!.saving} />
          ))}
          {settings.data.businesses.map((b) => (
            <PriceBotCard key={`price-${b.id}`} id={b.id} bot={b.price_bot} whatsapp={settings.data!.whatsapp} canSave={settings.data!.saving} />
          ))}
        </div>
      ) : null}
    </>
  );
}

function DeskCard({ id, name, desk, music, canSave }: { id: string; name: string; desk: DeskSettings; music: string[]; canSave: boolean }) {
  const toast = useToast();
  const [numbers, setNumbers] = useState<string[]>(desk.numbers.length ? desk.numbers : [""]);
  const custom = !music.includes(desk.hold_music);
  const [musicChoice, setMusicChoice] = useState(custom ? "custom" : desk.hold_music);
  const [musicUrl, setMusicUrl] = useState(custom ? desk.hold_music : "");
  const [wait, setWait] = useState(desk.max_wait_seconds);
  const [callerId, setCallerId] = useState(desk.caller_id ?? "");
  const [problems, setProblems] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);

  const edited = <T,>(set: (v: T) => void) => (v: T) => {
    setProblems([]);
    set(v);
  };

  const save = async () => {
    setSaving(true);
    setProblems([]);
    const body = {
      numbers: numbers.map((n) => n.trim()).filter(Boolean),
      hold_music: musicChoice === "custom" ? musicUrl.trim() : musicChoice,
      max_wait_seconds: Number(wait),
      caller_id: callerId.trim() || null,
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
        toast("ההגדרות נשמרו. הן חלות על השיחות הבאות.", "good");
      } else if (res.status === 400) {
        const e = (await res.json()) as { problems?: string[] };
        setProblems(e.problems ?? ["ההגדרות לא תקינות"]);
      } else {
        toast(`לא נשמר (${res.status})`, "bad");
      }
    } catch (e) {
      if (!(e instanceof Unauthorized)) toast("אין חיבור לשרת", "bad");
    } finally {
      setSaving(false);
    }
  };

  const active = numbers.some((n) => n.trim());

  return (
    <Card
      title={
        <span className="inline-flex items-center gap-2">
          <PhoneForwarded className="size-4 text-brand-600 dark:text-brand-300" aria-hidden />
          העברה למוקד שירות{name ? ` · ${name}` : ""}
        </span>
      }
      action={active ? <Badge tone="good" dot>פעיל</Badge> : <Badge tone="warn" dot>לא מוגדר</Badge>}
    >
      <div className="grid gap-7">
        <label className="grid gap-2 text-sm">
          מספר זיהוי יוצא למוקד
          <Input type="tel" dir="ltr" value={callerId} onChange={(e) => edited(setCallerId)(e.target.value)} placeholder="+972501234567" />
          <span className="text-slate-500">מספר מאושר ב-Twilio. ריק: מספר העסק שאליו התקשרו.</span>
        </label>
        <fieldset className="grid gap-2.5">
          <legend className="mb-1 text-sm font-medium">מספרי המוקד</legend>
          {numbers.map((n, i) => (
            <div key={i} className="flex items-center gap-2">
              <span className="flex size-10 shrink-0 items-center justify-center rounded-xl bg-slate-100 text-slate-500 dark:bg-white/[0.07] dark:text-slate-400">
                <PhoneOutgoing className="size-4" aria-hidden />
              </span>
              <Input
                type="tel"
                dir="ltr"
                inputMode="tel"
                aria-label={`מספר ${i + 1}`}
                placeholder="050-1234567"
                value={n}
                onChange={(e) => edited(setNumbers)(numbers.map((x, j) => (j === i ? e.target.value : x)))}
                className="num text-right"
              />
              <Button variant="ghost" aria-label="הסרת המספר" className="px-2.5" onClick={() => edited(setNumbers)(numbers.length > 1 ? numbers.filter((_, j) => j !== i) : [""])}>
                <Trash2 className="size-4" aria-hidden />
              </Button>
            </div>
          ))}
          <div>
            <Button variant="ghost" onClick={() => edited(setNumbers)([...numbers, ""])} disabled={numbers.length >= 10} className="-ms-2 text-brand-700 dark:text-brand-300">
              <Plus className="size-4" aria-hidden />
              מספר נוסף
            </Button>
          </div>
          <p className="text-xs leading-relaxed text-slate-500 dark:text-slate-400">אפשר לכתוב 050-1234567 או ‎+972501234567. בלי מספרים, המתקשר ישמע שאין מוקדן פנוי.</p>
        </fieldset>

        <div className="grid gap-5 sm:grid-cols-2">
          <Field label="מוזיקת המתנה" hint={musicChoice === "custom" ? "קישור מלא לקובץ שמע, שמתחיל ב-https://" : "מה המתקשר שומע עד שמישהו עונה."}>
            {(fid) => (
              <>
                <select id={fid} value={musicChoice} onChange={(e) => edited(setMusicChoice)(e.target.value)} className={cx(FIELD, "cursor-pointer")}>
                  {music.map((m) => (
                    <option key={m} value={m}>
                      {MUSIC[m] ?? m}
                    </option>
                  ))}
                  <option value="custom">קובץ משלי (קישור https)</option>
                </select>
                {musicChoice === "custom" && (
                  <Input type="url" dir="ltr" aria-label="קישור לקובץ המוזיקה" placeholder="https://…/music.mp3" value={musicUrl} onChange={(e) => edited(setMusicUrl)(e.target.value)} />
                )}
              </>
            )}
          </Field>

          <Field label="כמה זמן לחכות למענה" hint="אם אף אחד לא עונה עד אז, המתקשר שומע שאין מוקדן פנוי והשיחה מסתיימת.">
            {(fid) => (
              <div className="relative">
                <Input id={fid} type="number" min={15} max={600} value={wait} onChange={(e) => edited(setWait)(Number(e.target.value))} className="num pe-16" />
                <span className="pointer-events-none absolute inset-y-0 end-3.5 flex items-center text-sm text-slate-500">שניות</span>
              </div>
            )}
          </Field>
        </div>

        {problems.length > 0 && <Problem>{problems.join(" · ")}</Problem>}

        <div className="flex flex-wrap items-center gap-3 border-t border-slate-100 pt-5 dark:border-white/[0.06]">
          <Button variant="primary" onClick={save} disabled={!canSave || saving} className="min-w-28">
            {saving ? "שומר…" : "שמירה"}
          </Button>
          {!canSave && <span className="text-sm text-slate-500">אין מסד נתונים: אי אפשר לשמור</span>}
        </div>
      </div>
    </Card>
  );
}
