// The call flow, written out: what the agent does and which rules hold it in place.

import { useMemo, useState } from "react";
import { Ban, CarTaxiFront, CheckCheck, Headset, MapPin, MessagesSquare, Mic, Search, ShieldCheck, Zap } from "lucide-react";
import { Badge, Card, Empty, Input, PageHeader, Segmented, cx } from "../ui";

/** Who holds the rule: the code (the model cannot get around it) or the prompt (the model is told to). */
type Kind = "code" | "prompt";
type Rule = { text: string; kind?: Kind };
type Section = { id: string; title: string; note?: string; icon: typeof MapPin; rules: Rule[] };

const SECTIONS: Section[] = [
  {
    id: "flow",
    title: "סדר ההזמנה",
    note: "אפשר לתת פרטים בכל סדר. המערכת שואלת רק על מה שחסר.",
    icon: CarTaxiFront,
    rules: [
      { text: "איסוף ואז יעד. אף פעם לא שאלה אחת על שני מקומות.", kind: "prompt" },
      { text: "מספר נוסעים.", kind: "prompt" },
      { text: "שם להזמנה. מדלגים עליו כשהלקוח מוכר.", kind: "code" },
      { text: "שאלה אחת לנהג (״יש משהו שהנהג צריך לדעת?״) לפני ההקראה, גם אם המודל דילג עליה.", kind: "code" },
      { text: "הקראה ו״כן״. זמן האיסוף הוא ״עכשיו״ אם לא נאמר אחרת.", kind: "code" },
    ],
  },
  {
    id: "address",
    title: "כתובות",
    note: "החלק הכי מפותח. כל מקום נבדק מול הרשימה הרשמית של יישובים ורחובות בישראל.",
    icon: MapPin,
    rules: [
      { text: "בדיקה מול רשימת יישובים ורחובות, ומול רשימת מקומות שאינם רחובות (בנייני האומה, קניונים ועוד).", kind: "code" },
      { text: "שלוש שאלות נפרדות לכל מקום: עיר, אחר כך רחוב, אחר כך מספר בית. כל שאלה אומרת על איזה מקום (״איפה באלעד לאסוף?״).", kind: "prompt" },
      { text: "רחוב נבדק בעיר שכבר נאמרה: ״בן זכאי 45״ אחרי ״אלעד״ הוא רחוב באלעד ולא המושב בן זכאי. ״ביתר״ הוא ביתר עילית.", kind: "code" },
      { text: "מספר בית שנאמר במילים (״ארבעים וחמש״) הופך ל-45 לפני הבדיקה.", kind: "code" },
      { text: "רחוב עם מספר אבל בלי עיר: שואלים באיזו עיר, והרחוב נשמר.", kind: "code" },
      { text: "רחוב שלא נמצא בעיר: מבקשים שוב פעם אחת. ״כן״ להצעה קרובה הוא אותו רחוב, ואם המתקשר מתעקש, הרחוב נשמר כמו שאמר.", kind: "code" },
      { text: "מקום שאינו רחוב מתקבל. אם הוא לא ברשימה, שואלים פעם אחת ״יש כתובת של המקום?״.", kind: "code" },
      { text: "איסוף או יעד לפי מ/ל: כשלא ברור לאיזה מהם הכוונה, שואלים. לא מנחשים ולא מעבירים עיר מהאחד לשני.", kind: "prompt" },
      { text: "תיקון עיר (״לא, בבני ברק״): העיר מוחלפת וחוזרים לרחוב שלה.", kind: "prompt" },
      { text: "רחוב שהמודל השלים בעצמו ולא נאמר בשיחה נדחה.", kind: "code" },
      { text: "מבטא אשכנזי ושיבושי זיהוי: המודל מתקן (״אהרוינוביטש״ הופך ל״אהרונוביץ׳״), ואם אינו בטוח, שואל.", kind: "prompt" },
      { text: "״לא יודע״: ברחוב, העיר מספיקה. במספר בית, הרחוב נשמר כמו שהוא.", kind: "prompt" },
      { text: "אותו מקום נכשל שלוש פעמים: העברה למוקדן עם מה שנאסף. אם אין מוקד, בפעם השלישית המקום מתקבל כמו שנאמר ומסומן לנהג.", kind: "code" },
    ],
  },
  {
    id: "details",
    title: "נוסעים, שם והערות",
    icon: CarTaxiFront,
    rules: [
      { text: "בין 1 ל-20 נוסעים.", kind: "code" },
      { text: "מעל 4 נוסעים המערכת קובעת ואן. מעל 8 היא מעבירה למוקדן.", kind: "code" },
      { text: "הערה לנהג (מזוודות, כיסא ילד, ״תתקשר כשאתה מגיע״) נשמרת בשדה ההערות, ולא שואלים ״עוד משהו?״.", kind: "prompt" },
      { text: "המודל מתקן שמות שהזיהוי שיבש, והשם חייב להיות שם של אדם ולא משפט.", kind: "prompt" },
    ],
  },
  {
    id: "confirm",
    title: "אישור ושליחה",
    icon: CheckCheck,
    rules: [
      { text: "שליחה רק אחרי הקראה ו״כן״ שהמתקשר באמת אמר (כן, יאללה, תשלח, סבבה). מילה שהזיהוי המציא לא נחשבת.", kind: "code" },
      { text: "״כן אבל…״ ו״כן רגע…״ אינם ״כן״: מקריאים שוב או מתקנים. ״כן סליחה״ ו״כן, לא צריך כלום״ הם ״כן״.", kind: "code" },
      { text: "״כן״, ״אהה״ או ״בסדר״ תוך כדי ההקראה הם הקשבה: ההקראה לא נעצרת וההזמנה לא נשלחת. אם ההקראה נקטעה, ״כן״ מקריא אותה שוב.", kind: "code" },
      { text: "תיקון אחרי ההקראה: אותו פרט מתוקן ומקריאים הכול שוב.", kind: "code" },
      { text: "אחרי שההזמנה נשלחה אי אפשר לשנות אותה במערכת. המודל מעביר למוקדן ולא מבטיח ״אני אתקן״.", kind: "prompt" },
      { text: "זמן הגעה ומחיר אסורים בהבטחה. הם באים רק מהמערכת.", kind: "prompt" },
    ],
  },
  {
    id: "control",
    title: "שליטה בשיחה",
    icon: ShieldCheck,
    rules: [
      { text: "אותה שאלה לא נשאלת פעמיים ברצף. אחרי 3 חזרות המערכת מבקשת מהמודל לנסח אחרת, ואחרי 5 מעבירה למוקדן.", kind: "code" },
      { text: "אם המודל שואל שוב על פרט שהמתקשר בדיוק נתן, המנוע עובר לשאלה הבאה.", kind: "code" },
      { text: "לא עוברים לשאלה הבאה בלי תשובה לשאלה הפתוחה.", kind: "code" },
      { text: "פרט שלא נשאל עליו נשאר כמו שהיה, אלא אם המתקשר תיקן אותו במפורש (״לא״, ״רגע״, ״התכוונתי״).", kind: "code" },
      { text: "שאלה מוקלטת שאחריה שאלה שנייה בניסוח אחר: השנייה נזרקת.", kind: "code" },
      { text: "תשובה שהתחילה לפני שהשאלה הבאה נשמעה (״דוד״ ואז ״אביטבול״): מחברים אותן, רק אם ההפסקה ביניהן קצרה.", kind: "code" },
      { text: "המתקשר הוסיף משהו אחרי שהסוכן כבר התחיל לענות: שני המשפטים נשלחים יחד, ושום פרט לא נאבד ולא נשאל שוב.", kind: "code" },
      { text: "״טוב״ או ״תודה״ כשהשיחה מחכה לתשובה קצרה (הקראה, ״משהו נוסף?״, שאלה לנהג) הם תשובה ולא רעש. ״תודה״ אחרי ״משהו נוסף?״ מסיים את השיחה.", kind: "code" },
      { text: "״אתה שומע?״ או ״הלו?״: עונים ״כן, אני פה.״ ושואלים שוב את השאלה האחרונה.", kind: "code" },
      { text: "״רגע״: ״בטח, אני מחכה.״, ו-15 שניות עד ״הלו, שומעים אותי?״.", kind: "code" },
      { text: "דיבור שלא מופנה לסוכן ועלבונות: תשובה קצרה וחוזרים להזמנה, בלי לנתק.", kind: "prompt" },
      { text: "השיחה מסתיימת רק על פרידה אמיתית, או ״לא תודה״ אחרי ״משהו נוסף?״.", kind: "code" },
      { text: "המודל נכשל או לא ענה תוך 4 שניות: החוקים הקבועים מטפלים בתור הזה.", kind: "code" },
      { text: "כשצריך לשאול שוב, אומרים למה: ״סליחה, יש קצת רעש בקו.״ ואז השאלה. אף פעם לא ״לא הבנתי״.", kind: "prompt" },
    ],
  },
  {
    id: "audio",
    title: "קול והאזנה",
    icon: Mic,
    rules: [
      { text: "סוף דיבור מזוהה אחרי בערך חצי שנייה של שקט.", kind: "code" },
      { text: "הסוכן נעצר רק כשהמתקשר באמת מדבר: מילים, או קול של חצי שנייה (בזמן הקראה 1.2 שניות). שיעול, צופר או טלוויזיה לא קוטעים אותו.", kind: "code" },
      { text: "קול בלי מילים, או רעש שקטע את הסוכן: ״סליחה, יש קצת רעש בקו.״ ושוב השאלה, או מה שנקטע. ההתנצלות נאמרת פעם אחת בתור.", kind: "code" },
      { text: "משפט קטוע (״ואני רוצה להגיע ל…״) ממתין 1.2 שניות להמשך.", kind: "code" },
      { text: "מילים בודדות שהזיהוי ממציא מרעש (״תודה.״) מסוננות.", kind: "code" },
      { text: "שקט של 5 שניות: ״הלו, שומעים אותי?״, עד פעמיים. כשכבר נמסרו פרטים, עוד פעמיים ״אני עדיין פה, אפשר לקחת את הזמן.״ כל 15 שניות, ורק אז סיום.", kind: "code" },
      { text: "הפנייה נשארת ניטרלית עד שהמתקשר מדבר על עצמו בזכר או בנקבה, ואז מתאימה. אף פעם לא שואלים ״גבר או אישה?״.", kind: "prompt" },
      { text: "מקסימום 15 דקות לשיחה.", kind: "code" },
    ],
  },
  {
    id: "desk",
    title: "העברה למוקדן",
    note: "המספרים והמוזיקה מוגדרים בדף ההגדרות.",
    icon: Headset,
    rules: [
      { text: "מתי: המתקשר מבקש נציג או מתלונן, קבוצה מעל 8, כתובת שלא הובנה 3 פעמים, 5 שאלות זהות, או הזמנה שלא ידוע אם נקלטה.", kind: "code" },
      { text: "המתקשר שומע מוזיקה, כל מספרי המוקד מצלצלים יחד, והראשון שעונה שומע סיכום ומקבל אותו.", kind: "code" },
      { text: "אם כולם נכשלים או לא עונים, המתקשר שומע מיד ״אין מוקדן פנוי״ ולא ממתין.", kind: "code" },
      { text: "מתקשר שניתק בזמן ההמתנה: השיחות למוקדנים מבוטלות מיד.", kind: "code" },
    ],
  },
  {
    id: "actions",
    title: "שליחת ההזמנה",
    icon: Ban,
    rules: [
      { text: "כל פעולה נושאת מפתח שלא משתנה בין ניסיונות, כך שניסיון חוזר לא יוצר הזמנה כפולה.", kind: "code" },
      { text: "תקלת זמן אחרי ששלחנו נחשבת ״לא ידוע אם נקלט״ ולא ״נכשל״. הכרטיס מסומן לבדיקה והמתקשר עובר למוקדן.", kind: "code" },
      { text: "כרטיס עם כל הפרטים נשמר בדף ההזמנות ונשלח לוואטסאפ.", kind: "code" },
    ],
  },
  {
    id: "speed",
    title: "מהירות",
    icon: Zap,
    rules: [
      { text: "שאלות קבועות מושמעות מהקלטה מיד (המודל בוחר בהן לפי מזהה). שאר המשפטים בקול חי.", kind: "code" },
      { text: "״אממ…״ אחרי כ-1.1 שניות אם המודל עוד חושב.", kind: "code" },
      { text: "אם המודל הראשי לא ענה תוך 1.2 שניות, מודל הגיבוי מקבל את אותה בקשה.", kind: "code" },
      { text: "אם שני המודלים של OpenAI נכשלים, מודל של ספק אחר (Gemini) עונה, כך שתקלה אצל ספק אחד לא מפילה שיחות. לזיהוי הקול יש גיבוי ב-Deepgram.", kind: "code" },
    ],
  },
];

const KIND: Record<Kind, { label: string; tone: "brand" | "warn"; hint: string }> = {
  code: { label: "בקוד", tone: "brand", hint: "המודל לא יכול לעקוף" },
  prompt: { label: "הנחיה", tone: "warn", hint: "המודל מקבל הוראה ובדרך כלל מציית" },
};

type Filter = "all" | Kind;

export function Rules() {
  const [filter, setFilter] = useState<Filter>("all");
  const [query, setQuery] = useState("");

  const shown = useMemo(() => {
    const q = query.trim();
    return SECTIONS.map((s) => ({
      ...s,
      rules: s.rules.filter((r) => (filter === "all" || r.kind === filter) && (!q || r.text.includes(q) || s.title.includes(q))),
    })).filter((s) => s.rules.length > 0);
  }, [filter, query]);

  const total = SECTIONS.reduce((n, s) => n + s.rules.length, 0);
  const count = (k: Kind) => SECTIONS.reduce((n, s) => n + s.rules.filter((r) => r.kind === k).length, 0);

  return (
    <>
      <PageHeader title="כללי שיחה" subtitle="כל מה שהסוכן עושה בשיחה, ומי אוכף את זה" />

      <div className="mb-6 grid gap-3 sm:grid-cols-3">
        <div className="flex gap-3 rounded-2xl bg-brand-50/70 p-4 ring-1 ring-inset ring-brand-100 sm:col-span-2 dark:bg-brand-400/[0.07] dark:ring-brand-400/15">
          <span className="flex size-9 shrink-0 items-center justify-center rounded-xl bg-white text-brand-600 shadow-sm dark:bg-white/10 dark:text-brand-200">
            <MessagesSquare className="size-[18px]" aria-hidden />
          </span>
          <p className="text-sm leading-relaxed text-slate-700 dark:text-slate-200">
            המודל מציע מה לומר ומה להבין, והמנוע מחליט מה מותר. שום דבר לא נשלח בלי הקראה ו״כן״ אמיתי, והשיחה לא מסתיימת בלי פרידה אמיתית.
          </p>
        </div>
        <div className="grid gap-2 rounded-2xl bg-white p-4 text-xs shadow-card ring-1 ring-slate-200/70 dark:bg-slate-900/70 dark:shadow-none dark:ring-white/[0.08]">
          {(Object.keys(KIND) as Kind[]).map((k) => (
            <div key={k} className="flex items-center gap-2">
              <Badge tone={KIND[k].tone}>{KIND[k].label}</Badge>
              <span className="text-slate-600 dark:text-slate-300">{KIND[k].hint}</span>
            </div>
          ))}
        </div>
      </div>

      <div className="mb-6 flex flex-wrap items-center gap-3">
        <div className="relative min-w-56 flex-1 sm:max-w-xs">
          <Search className="pointer-events-none absolute start-3 top-1/2 size-4 -translate-y-1/2 text-slate-400" aria-hidden />
          <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="חיפוש בכללים" aria-label="חיפוש בכללים" className="ps-9" />
        </div>
        <Segmented
          label="מי אוכף"
          value={filter}
          onChange={setFilter}
          options={[
            { value: "all", label: "הכול", count: total },
            { value: "code", label: "בקוד", count: count("code") },
            { value: "prompt", label: "הנחיה", count: count("prompt") },
          ]}
        />
      </div>

      {shown.length === 0 ? (
        <Card>
          <Empty icon={<Search className="size-5" aria-hidden />}>לא נמצא כלל שמתאים לחיפוש.</Empty>
        </Card>
      ) : (
        <div className="grid items-start gap-6 lg:grid-cols-2">
          {shown.map(({ id, title, note, icon: Icon, rules }) => (
            <Card
              key={id}
              title={
                <span className="flex items-center gap-2.5">
                  <span className="flex size-8 items-center justify-center rounded-lg bg-brand-50 text-brand-600 dark:bg-brand-400/15 dark:text-brand-200">
                    <Icon className="size-4" aria-hidden />
                  </span>
                  {title}
                </span>
              }
              subtitle={note}
            >
              <ul className="grid gap-2.5">
                {rules.map((r) => (
                  <li key={r.text} className="flex items-start gap-3 text-sm leading-relaxed text-slate-700 dark:text-slate-200">
                    <span className={cx("mt-2 size-1.5 shrink-0 rounded-full", r.kind === "prompt" ? "bg-amber-500" : "bg-brand-500")} aria-hidden />
                    <span className="flex-1">{r.text}</span>
                    {r.kind && (
                      <span className="mt-0.5 shrink-0">
                        <Badge tone={KIND[r.kind].tone}>{KIND[r.kind].label}</Badge>
                      </span>
                    )}
                  </li>
                ))}
              </ul>
            </Card>
          ))}
        </div>
      )}

      <p className="mt-6 max-w-3xl text-xs leading-relaxed text-slate-500 dark:text-slate-400">
        כללי ״הנחיה״ תלויים בציות של המודל. אנחנו בודקים אותם ב-eval ובדף השיחות, שם רואים איפה הוא סטה.
      </p>
    </>
  );
}
