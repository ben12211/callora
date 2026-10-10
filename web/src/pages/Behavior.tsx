// The behavior page: what the agent is told (its system prompt), the sentences it says, the words
// the system listens for, timings and limits, and places. Each item shows the file's value until
// the owner changes it; a change is checked like the file is, used by the next call, and can be
// taken back to the file's value. A changed sentence is recorded in the voice in use.

import { useMemo, useState, type ReactNode } from "react";
import { Copy, RotateCcw, Search } from "lucide-react";
import { type Session, Unauthorized, useApi } from "../api";
import { Badge, Button, Card, FIELD, Input, PageHeader, Problem, Segmented, Skeleton, cx, useToast } from "../ui";

type Rule = { id: string; when?: { gt?: number }; [k: string]: unknown };
type View = {
  agent: { persona: string; rules: string[]; timeout_ms: number; filler_after_ms: number };
  responses: Record<string, string[]>;
  lexicon: Record<string, string[]>;
  meta_intents: Record<string, { exact?: string[] | null; phrases?: string[] | null }>;
  silence: Record<string, number | string | null>;
  voice: { tempo: number };
  slots: { passengers: { max: number | null } };
  rules: Rule[];
  service_area: string[];
  stt_keyterms: string[];
  personal_places: string[];
  informal_places: string[];
};
type Config = { file: View; current: View; changed: string[]; prompt: string | null; saving: boolean; recording: boolean };

type Save = (path: string, value: unknown) => Promise<boolean>;

const SECTIONS = [
  { value: "agent", label: "הנחיות לסוכן" },
  { value: "responses", label: "משפטים" },
  { value: "words", label: "מילים" },
  { value: "limits", label: "זמנים ומגבלות" },
  { value: "places", label: "מקומות" },
] as const;
type Section = (typeof SECTIONS)[number]["value"];

const RESPONSE_LABELS: Record<string, string> = {
  greeting: "פתיחת השיחה",
  ack: "אישור קצר",
  goodbye: "פרידה",
  anything_else: "\"משהו נוסף?\"",
  still_there: "שקט: \"הלו, שומעים?\"",
  still_waiting: "שקט אחרי שנמסרו פרטים",
  noisy_line: "רעש בקו",
  did_not_catch: "לא שמעתי",
  abuse_warning: "קללה ראשונה",
  abuse_goodbye: "קללה שנייה: סיום השיחה",
  i_hear_you: "\"אתה שומע?\"",
  wait_ack: "\"רגע\"",
  handoff: "העברה למוקדן",
  handoff_unavailable: "אין מוקדן פנוי",
  ask_route: "\"מאיפה לאן?\"",
  ask_pickup: "שאלת איסוף",
  ask_destination: "שאלת יעד",
  ask_passengers: "שאלת נוסעים",
  ask_name: "שאלת שם",
  ask_notes: "שאלה לנהג",
  confirm_ride: "הקראת ההזמנה",
  confirm_ride_with_name: "הקראת ההזמנה עם שם",
  ride_booked: "ההזמנה נשלחה",
  ride_failed: "ההזמנה נכשלה",
  off_topic: "נושא לא קשור",
  small_talk_answer: "\"מה נשמע?\"",
  filler_thinking: "\"אממ...\" בזמן חשיבה",
  filler_searching: "\"רושם...\"",
  price_answer: "תשובת מחיר",
  price_failed: "אין מחיר",
  personal_place_address: "\"מה הכתובת של הבית?\"",
  large_group: "קבוצה גדולה",
  hours_answer: "שעות פעילות",
  invoice_answer: "חשבונית",
};

const LEXICON_LABELS: Record<string, [string, string]> = {
  affirm: ["\"כן\"", "מילים שנחשבות הסכמה בהקראה."],
  deny: ["\"לא\"", "מילים שנחשבות סירוב."],
  now: ["\"עכשיו\"", "מילים שאומרות שהנסיעה מיידית."],
  correct: ["תיקון", "מילים של מתקשר שמתקן פרט (\"טעיתי\", \"רגע\")."],
  same_city: ["נסיעה פנימית", "מילים שאומרות שהיעד באותה עיר של האיסוף."],
  abuse: ["קללות", "בפעם הראשונה אזהרה, בשנייה השיחה מסתיימת. שום דבר מהמשפט לא נשמר."],
  hello: ["\"הלו\"", "בדיקה שהקו פתוח: עונים \"כן, אני פה\"."],
  nothing: ["\"אין\"", "תשובה ריקה לשאלה לא חובה (\"אין הערות\")."],
  harmless: ["לא תיקון", "ביטויים עם מילת תיקון שלא מתקנים כלום (\"סליחה\")."],
  fillers: ["מילות מילוי", "מילים בלי תוכן (\"אממ\", \"אחי\")."],
  transfer: ["מילות העברה", "מילים שהסוכן אומר רק כשבאמת מעביר למוקדן."],
};

const META_LABELS: Record<string, string> = {
  goodbye: "פרידה (מסיים את השיחה)",
  wait: "\"רגע\" (הסוכן מחכה)",
  transfer_human: "בקשה לנציג",
  cancel_current_flow: "ביטול",
  go_back: "חזרה אחורה",
  repeat_last: "\"מה אמרת?\"",
  did_not_understand: "\"לא הבנתי\"",
  speak_slower: "\"יותר לאט\"",
  speak_louder: "\"יותר חזק\"",
};

export function Behavior({ session }: { session: Session }) {
  const id = session.businesses[0]?.id;
  const config = useApi<Config>(id ? `/api/config/${encodeURIComponent(id)}` : null, undefined);
  const toast = useToast();
  const [section, setSection] = useState<Section>("agent");
  const [shown, setShown] = useState<Config | null>(null);
  const c = shown ?? config.data;

  const save: Save = async (path, value) => {
    try {
      const res = await fetch(`/api/config/${encodeURIComponent(id ?? "")}`, {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ path, value }),
      });
      if (res.status === 401) {
        window.dispatchEvent(new Event("callora:unauthorized"));
        throw new Unauthorized("unauthorized");
      }
      if (!res.ok) {
        const e = (await res.json().catch(() => ({}))) as { problems?: string[] };
        toast(e.problems?.length ? `לא נשמר: ${e.problems.join(" · ")}` : `לא נשמר (${res.status})`, "bad");
        return false;
      }
      setShown((await res.json()) as Config);
      toast(value === null ? "חזר לברירת המחדל." : "נשמר. חל מהשיחה הבאה.", "good");
      return true;
    } catch (e) {
      if (!(e instanceof Unauthorized)) toast("אין חיבור לשרת", "bad");
      return false;
    }
  };

  return (
    <>
      <PageHeader
        title="התנהגות הסוכן"
        subtitle="מה הסוכן יודע ואומר. כל שינוי נבדק, חל מהשיחה הבאה, ואפשר תמיד להחזיר לברירת המחדל."
        action={<Segmented label="אזור" value={section} options={[...SECTIONS]} onChange={setSection} />}
      />
      {config.error && <Problem>{config.error}</Problem>}
      {!c && !config.error && <Skeleton className="h-96 rounded-2xl" />}
      {c && (
        <div className="grid max-w-4xl gap-6">
          {!c.saving && <Problem>אין מסד נתונים בשרת: אפשר לראות, אי אפשר לשמור.</Problem>}
          {c.changed.length > 0 && (
            <p className="text-sm text-slate-500 dark:text-slate-400">
              <Badge tone="brand">{c.changed.length}</Badge> פריטים שונו מברירת המחדל.
            </p>
          )}
          {section === "agent" && <AgentSection c={c} save={save} />}
          {section === "responses" && <ResponsesSection c={c} save={save} />}
          {section === "words" && <WordsSection c={c} save={save} />}
          {section === "limits" && <LimitsSection c={c} save={save} />}
          {section === "places" && <PlacesSection c={c} save={save} />}
        </div>
      )}
    </>
  );
}

/** One item: its label, whether it was changed, a way back to the file's value, and its editor. */
function Item({ c, path, label, hint, save, children }: { c: Config; path: string; label: ReactNode; hint?: ReactNode; save: Save; children: ReactNode }) {
  const changed = c.changed.includes(path);
  return (
    <div className="grid gap-2 border-b border-slate-100 pb-5 last:border-0 last:pb-0 dark:border-white/[0.06]">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm font-semibold text-slate-900 dark:text-white">{label}</span>
        {changed && <Badge tone="brand">שונה</Badge>}
        {changed && c.saving && (
          <Button variant="ghost" className="ms-auto px-2 py-1 text-xs" onClick={() => void save(path, null)} title="חזרה לערך שבקובץ">
            <RotateCcw className="size-3.5" aria-hidden />
            ברירת מחדל
          </Button>
        )}
      </div>
      {hint && <p className="text-xs leading-relaxed text-slate-500 dark:text-slate-400">{hint}</p>}
      {children}
    </div>
  );
}

function SaveRow({ dirty, onSave, onCancel, can }: { dirty: boolean; onSave: () => void; onCancel: () => void; can: boolean }) {
  if (!dirty) return null;
  return (
    <div className="flex gap-2">
      <Button variant="primary" onClick={onSave} disabled={!can}>
        שמירה
      </Button>
      <Button variant="ghost" onClick={onCancel}>
        ביטול
      </Button>
    </div>
  );
}

/** A long text (the persona). */
function TextEditor({ value, rows = 6, save, can }: { value: string; rows?: number; save: (v: string) => Promise<boolean>; can: boolean }) {
  const [text, setText] = useState(value);
  const dirty = text !== value;
  return (
    <>
      <textarea dir="auto" rows={rows} value={text} onChange={(e) => setText(e.target.value)} className={cx(FIELD, "leading-relaxed")} />
      <SaveRow dirty={dirty} can={can} onCancel={() => setText(value)} onSave={() => void save(text)} />
    </>
  );
}

/** Short items, one per line (words, sentences, towns). */
function LinesEditor({ value, save, can, rows }: { value: string[] | null | undefined; save: (v: string[]) => Promise<boolean>; can: boolean; rows?: number }) {
  const initial = (value ?? []).join("\n");
  const [text, setText] = useState(initial);
  const dirty = text !== initial;
  const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
  return (
    <>
      <textarea
        dir="auto"
        rows={rows ?? Math.min(Math.max((value ?? []).length, 2), 10)}
        value={text}
        onChange={(e) => setText(e.target.value)}
        className={cx(FIELD, "leading-relaxed")}
      />
      <SaveRow dirty={dirty} can={can} onCancel={() => setText(initial)} onSave={() => void save(lines)} />
    </>
  );
}

/** Long items each in its own box (the agent's rules). */
function RulesEditor({ value, save, can }: { value: string[]; save: (v: string[]) => Promise<boolean>; can: boolean }) {
  const [rules, setRules] = useState(value);
  const dirty = JSON.stringify(rules) !== JSON.stringify(value);
  return (
    <div className="grid gap-3">
      {rules.map((r, i) => (
        <div key={i} className="flex items-start gap-2">
          <span className="num mt-2.5 w-6 shrink-0 text-xs text-slate-400">{i + 1}.</span>
          <textarea
            dir="auto"
            rows={Math.min(Math.ceil(r.length / 110) + 1, 8)}
            value={r}
            onChange={(e) => setRules(rules.map((x, j) => (j === i ? e.target.value : x)))}
            className={cx(FIELD, "text-[13px] leading-relaxed")}
          />
          <Button variant="ghost" className="px-2" onClick={() => setRules(rules.filter((_, j) => j !== i))} title="מחיקת ההנחיה">
            ✕
          </Button>
        </div>
      ))}
      <div>
        <Button variant="ghost" onClick={() => setRules([...rules, ""])}>
          + הנחיה חדשה
        </Button>
      </div>
      <SaveRow dirty={dirty} can={can} onCancel={() => setRules(value)} onSave={() => void save(rules.map((r) => r.trim()).filter(Boolean))} />
    </div>
  );
}

/** A number, optionally shown in another unit (milliseconds as seconds). */
function NumberEditor({ value, save, can, scale = 1, unit, step = 1, min }: { value: number | null; save: (v: number) => Promise<boolean>; can: boolean; scale?: number; unit?: string; step?: number; min?: number }) {
  const initial = value == null ? "" : String(value / scale);
  const [text, setText] = useState(initial);
  const n = Number(text);
  const dirty = text !== initial;
  return (
    <div className="flex flex-wrap items-center gap-3">
      <div className="relative w-40">
        <Input type="number" step={step} min={min} value={text} onChange={(e) => setText(e.target.value)} className="num pe-14" />
        {unit && <span className="pointer-events-none absolute inset-y-0 end-3 flex items-center text-xs text-slate-500">{unit}</span>}
      </div>
      <SaveRow dirty={dirty} can={can && text !== "" && Number.isFinite(n)} onCancel={() => setText(initial)} onSave={() => void save(Math.round(n * scale * 1000) / 1000)} />
    </div>
  );
}

function AgentSection({ c, save }: { c: Config; save: Save }) {
  const toast = useToast();
  const key = (p: string) => `${p}:${JSON.stringify(c.changed.includes(p))}:${c.changed.length}`;
  return (
    <>
      <Card title="מי הסוכן" subtitle="איך הוא מדבר ומתנהג. נשלח למודל בתחילת כל שיחה.">
        <Item c={c} path="agent.persona" label="אישיות" save={save}>
          <TextEditor key={key("agent.persona")} value={c.current.agent.persona} rows={8} can={c.saving} save={(v) => save("agent.persona", v)} />
        </Item>
      </Card>
      <Card title="הנחיות" subtitle="הכללים שהמודל מקבל, אחד אחד. אפשר לערוך, למחוק ולהוסיף. כתובים באנגלית כי המודל מדייק בהם יותר, אבל אפשר לכתוב גם בעברית.">
        <Item c={c} path="agent.rules" label={`${c.current.agent.rules.length} הנחיות`} save={save}>
          <RulesEditor key={key("agent.rules")} value={c.current.agent.rules} can={c.saving} save={(v) => save("agent.rules", v)} />
        </Item>
      </Card>
      {c.prompt && (
        <Card
          title="הפרומפט המלא"
          subtitle="מה שהמודל מקבל בפועל: האישיות וההנחיות שלמעלה, יחד עם החלקים הקבועים של המערכת (לקריאה בלבד)."
          action={
            <Button
              variant="ghost"
              onClick={() =>
                navigator.clipboard.writeText(c.prompt ?? "").then(
                  () => toast("הועתק", "good"),
                  () => toast("לא הועתק", "bad"),
                )
              }
            >
              <Copy className="size-4" aria-hidden />
              העתקה
            </Button>
          }
        >
          <details>
            <summary className="cursor-pointer text-sm font-medium text-brand-700 dark:text-brand-300">הצג ({c.prompt.length.toLocaleString()} תווים)</summary>
            <pre dir="auto" className="mt-3 max-h-[32rem] overflow-auto whitespace-pre-wrap rounded-xl bg-slate-50 p-4 text-xs leading-relaxed text-slate-700 dark:bg-white/[0.04] dark:text-slate-300">
              {c.prompt}
            </pre>
          </details>
        </Card>
      )}
    </>
  );
}

function ResponsesSection({ c, save }: { c: Config; save: Save }) {
  const [query, setQuery] = useState("");
  const ids = useMemo(() => {
    const all = Object.keys(c.current.responses);
    const known = all.filter((id) => RESPONSE_LABELS[id]);
    const rest = all.filter((id) => !RESPONSE_LABELS[id]);
    const q = query.trim();
    return [...known, ...rest].filter((id) => !q || id.includes(q) || (RESPONSE_LABELS[id] ?? "").includes(q) || c.current.responses[id].some((v) => v.includes(q)));
  }, [c, query]);
  return (
    <Card
      title="משפטים קבועים"
      subtitle={
        <>
          מה הסוכן אומר ברגעים קבועים. כל שורה היא נוסח אחד, והסוכן מגוון ביניהם. {"{city}"} וכדומה מוחלפים בערך עצמו. משפט ששונה מוקלט מחדש בקול הנוכחי (כדקה, ועולה מעט קרדיטים).
          {c.recording && <Badge tone="warn">מקליט עכשיו…</Badge>}
        </>
      }
    >
      <div className="relative mb-5">
        <Search className="pointer-events-none absolute inset-y-0 start-3 my-auto size-4 text-slate-400" aria-hidden />
        <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="חיפוש משפט" aria-label="חיפוש משפט" className="ps-9" />
      </div>
      <div className="grid gap-5">
        {ids.map((id) => {
          const path = `responses.${id}.variants`;
          return (
            <Item key={id} c={c} path={path} label={RESPONSE_LABELS[id] ?? id} hint={RESPONSE_LABELS[id] ? <span dir="ltr">{id}</span> : undefined} save={save}>
              <LinesEditor key={`${path}:${c.current.responses[id].join("|")}`} value={c.current.responses[id]} can={c.saving} save={(v) => save(path, v)} />
            </Item>
          );
        })}
      </div>
    </Card>
  );
}

function WordsSection({ c, save }: { c: Config; save: Save }) {
  const lex = Object.keys(c.current.lexicon).sort((a, b) => (LEXICON_LABELS[a] ? 0 : 1) - (LEXICON_LABELS[b] ? 0 : 1));
  return (
    <>
      <Card title="מילים שהמערכת מזהה" subtitle="שורה לכל מילה או ביטוי. ההתאמה היא למילים שלמות, גם עם ה/ו/ב/ל/מ לפניהן.">
        <div className="grid gap-5">
          {lex.map((k) => {
            const path = `lexicon.${k}`;
            const [label, hint] = LEXICON_LABELS[k] ?? [k, undefined];
            return (
              <Item key={k} c={c} path={path} label={label} hint={hint} save={save}>
                <LinesEditor key={`${path}:${(c.current.lexicon[k] ?? []).join("|")}`} value={c.current.lexicon[k]} can={c.saving} save={(v) => save(path, v)} />
              </Item>
            );
          })}
        </div>
      </Card>
      <Card title="בקשות מיוחדות" subtitle="ביטויים שהמערכת מגיבה להם מיד, בלי המודל. ״מדויק״: כל המשפט הוא הביטוי. ״בתוך משפט״: הביטוי מופיע בו.">
        <div className="grid gap-5">
          {Object.keys(c.current.meta_intents).map((k) =>
            (["exact", "phrases"] as const)
              .filter((kind) => c.current.meta_intents[k][kind] != null)
              .map((kind) => {
                const path = `meta_intents.${k}.${kind}`;
                const value = c.current.meta_intents[k][kind] ?? [];
                return (
                  <Item key={path} c={c} path={path} label={`${META_LABELS[k] ?? k} · ${kind === "exact" ? "מדויק" : "בתוך משפט"}`} save={save}>
                    <LinesEditor key={`${path}:${value.join("|")}`} value={value} can={c.saving} save={(v) => save(path, v)} />
                  </Item>
                );
              }),
          )}
        </div>
      </Card>
    </>
  );
}

function LimitsSection({ c, save }: { c: Config; save: Save }) {
  const s = c.current.silence;
  const num = (v: number | string | null | undefined) => (typeof v === "number" ? v : null);
  const ruleGt = (id: string) => c.current.rules.find((r) => r.id === id)?.when?.gt ?? null;
  const saveRule = (id: string, gt: number) => save("rules", c.current.rules.map((r) => (r.id === id ? { ...r, when: { ...r.when, gt } } : r)));
  const k = (p: string, v: unknown) => `${p}:${JSON.stringify(v)}`;
  return (
    <>
      <Card title="שקט ותזמון">
        <div className="grid gap-5">
          <Item c={c} path="silence.reprompt_after_ms" label="שקט עד ״הלו, שומעים אותי?״" save={save}>
            <NumberEditor key={k("r", s.reprompt_after_ms)} value={num(s.reprompt_after_ms)} scale={1000} step={0.5} min={1} unit="שניות" can={c.saving} save={(v) => save("silence.reprompt_after_ms", v)} />
          </Item>
          <Item c={c} path="silence.max_reprompts" label="כמה פעמים לשאול ״הלו?״ לפני סיום" save={save}>
            <NumberEditor key={k("m", s.max_reprompts)} value={num(s.max_reprompts)} min={0} can={c.saving} save={(v) => save("silence.max_reprompts", v)} />
          </Item>
          <Item c={c} path="silence.patient_after_ms" label="המתנה אחרי ״רגע״" hint="ואחרי שנמסרו פרטים: הזמן בין ״אני עדיין פה״ אחד לשני." save={save}>
            <NumberEditor key={k("p", s.patient_after_ms)} value={num(s.patient_after_ms)} scale={1000} min={1} unit="שניות" can={c.saving} save={(v) => save("silence.patient_after_ms", v)} />
          </Item>
          <Item c={c} path="silence.patient_reprompts" label="כמה פעמים ״אני עדיין פה״" hint="רק כשכבר נמסרו פרטים, לפני שהשיחה מסתיימת." save={save}>
            <NumberEditor key={k("pr", s.patient_reprompts)} value={num(s.patient_reprompts)} min={0} can={c.saving} save={(v) => save("silence.patient_reprompts", v)} />
          </Item>
          <Item c={c} path="agent.filler_after_ms" label="אחרי כמה זמן ״אממ...״" hint="כשהמודל עוד חושב." save={save}>
            <NumberEditor key={k("f", c.current.agent.filler_after_ms)} value={c.current.agent.filler_after_ms} scale={1000} step={0.1} min={0.2} unit="שניות" can={c.saving} save={(v) => save("agent.filler_after_ms", v)} />
          </Item>
          <Item c={c} path="agent.timeout_ms" label="זמן מקסימלי להחלטת המודל" hint="אחריו הכללים הקבועים מחליטים על התור." save={save}>
            <NumberEditor key={k("t", c.current.agent.timeout_ms)} value={c.current.agent.timeout_ms} scale={1000} step={0.5} min={1} unit="שניות" can={c.saving} save={(v) => save("agent.timeout_ms", v)} />
          </Item>
          <Item c={c} path="voice.tempo" label="מהירות הדיבור" hint="1 הוא הקצב הרגיל של הקול, 1.2 מהיר ב-20%. שינוי מקליט מחדש את המשפטים הקבועים." save={save}>
            <NumberEditor key={k("v", c.current.voice.tempo)} value={c.current.voice.tempo} step={0.05} min={0.7} unit="×" can={c.saving} save={(v) => save("voice.tempo", v)} />
          </Item>
        </div>
      </Card>
      <Card title="נוסעים">
        <div className="grid gap-5">
          <Item c={c} path="slots.passengers.max" label="מקסימום נוסעים בהזמנה" save={save}>
            <NumberEditor key={k("x", c.current.slots.passengers.max)} value={c.current.slots.passengers.max} min={1} can={c.saving} save={(v) => save("slots.passengers.max", v)} />
          </Item>
          {ruleGt("van_for_large_groups") != null && (
            <Item c={c} path="rules" label="מעל כמה נוסעים מזמינים ואן" save={save}>
              <NumberEditor key={k("van", ruleGt("van_for_large_groups"))} value={ruleGt("van_for_large_groups")} min={1} can={c.saving} save={(v) => saveRule("van_for_large_groups", v)} />
            </Item>
          )}
          {ruleGt("very_large_group") != null && (
            <Item c={c} path="rules" label="מעל כמה נוסעים מעבירים למוקדן" save={save}>
              <NumberEditor key={k("big", ruleGt("very_large_group"))} value={ruleGt("very_large_group")} min={1} can={c.saving} save={(v) => saveRule("very_large_group", v)} />
            </Item>
          )}
        </div>
      </Card>
    </>
  );
}

function PlacesSection({ c, save }: { c: Config; save: Save }) {
  const lists: [keyof View, string, string][] = [
    ["service_area", "אזור השירות", "הערים שהמערכת מכירה הכי טוב: עוזר לזיהוי הקול ולהבנת הכתובות."],
    ["personal_places", "מקומות אישיים", "\"הבית\", \"אמא שלי\": הסוכן שואל מה הכתובת שלהם."],
    ["informal_places", "מקומות לא רשמיים", "\"צומת\", \"כניסה לעיר\", קניונים: מתקבלים גם בלי כתובת מדויקת."],
    ["stt_keyterms", "מילים לזיהוי הקול", "מילים שמנוע הזיהוי מקבל כרמז, כדי לשמוע אותן נכון."],
  ];
  return (
    <Card title="מקומות" subtitle="שורה לכל מקום או מילה.">
      <div className="grid gap-5">
        {lists.map(([key, label, hint]) => {
          const value = c.current[key] as string[];
          return (
            <Item key={key} c={c} path={key} label={label} hint={hint} save={save}>
              <LinesEditor key={`${key}:${value.join("|")}`} value={value} can={c.saving} save={(v) => save(key, v)} />
            </Item>
          );
        })}
      </div>
    </Card>
  );
}
