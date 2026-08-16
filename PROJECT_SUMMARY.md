# Callora — סיכום פרויקט

עדכון אחרון: 2026-08-14 · ענף: `claude/project-summary-md-2a5dae` · גרסה: `0.1.0` (חבילה: `callora-backend`)

## מה זה

Callora הוא backend רב-דיירי (multi-tenant) לשירות לקוחות טלפוני מבוסס AI. שירות Node.js/TypeScript יחיד מעל PostgreSQL. כל מספרי הטwilio מצביעים על אותו webhook; השירות מזהה את העסק לפי שדה `To` של השיחה הנכנסת, רושם את השיחה, ואז:

- אם לעסק יש שורת `agent_configs` פעילה — מחזיר TwiML של `<Connect><Stream>` ומגשר את השיחה לשיחת דיבור-לדיבור (speech-to-speech) מול OpenAI Realtime.
- אחרת — מחזיר ברכת `<Say>` סטטית של אותו עסק.

`From` נשמר רק אם הוא E.164 תקין, ולעולם לא משמש לבחירת דייר.

## סטאק

| שכבה | טכנולוגיה |
| --- | --- |
| ריצה | Node.js ≥20, TypeScript strict, ESM |
| HTTP | Fastify 5 + `@fastify/formbody` + `@fastify/websocket` |
| ולידציה | Zod 4 |
| טלפוניה | Twilio SDK 5 (TwiML, אימות חתימה, Media Streams) |
| AI | OpenAI Realtime דרך `ws` (ברירת מחדל `gpt-realtime-2.1`) |
| DB | PostgreSQL 16 + `pg`, מיגרציות SQL, seed אידמפוטנטי |
| איכות | Vitest (33 בדיקות), ESLint 9, `tsc` |
| פריסה | Docker multi-stage (arm64), Compose, Caddy, GitHub Actions |

מנהל חבילות: pnpm 10.28.2.

## מישור הבקרה (Push 1)

דשבורד ניהולי ב-`/dashboard`, מוגש כ-HTML מהשרת עצמו — אין frontend נפרד לבנות או לפרוס. מכיל התחברות, דף בית, ניהול עסקים (יצירה, עריכה, הפעלה, כיבוי, דף פירוט), טופס קונפיגורציית סוכן מלא (הפעלה, שפה, ברכה, הנחיות, ספק קול, קול, מודל), דף סטטוס ספקים, רשימת שיחות ודף שיחה, הגדרות עם החלפת סיסמה, והיסטוריית ביקורת.

האימות: עוגיית סשן בצד השרת (רק ה-SHA-256 נשמר) עם טוקן CSRF לכל טופס, או `ADMIN_API_KEY` בכותרת `X-Api-Key` לקריאות מכונה. ה-webhooks של Twilio נשארים מחוץ לכך ומאומתים בחתימה של Twilio כמקודם.

הזרימה: `התחברות → יצירת עסק → הגדרת סוכן → בחירת ספק וקול → שמירה → שיחה למספר`.

קלורה היא מקור האמת לקונפיגורציה; OpenAI, ElevenLabs ו-Cartesia הם ספקי הרצה בלבד, והקרדנציאלים שלהם נשארים ברמת הפלטפורמה ואינם נחשפים בדשבורד.

## ארכיטקטורה ומבנה הקוד

```text
src/
  server.ts                 עליית תהליך + כיבוי מסודר
  app.ts                    מפעל האפליקציה, רישום פלאגינים, error handler מרכזי
  config.ts                 קונפיגורציית סביבה מאומתת ב-Zod
  domain/models.ts          טיפוסי דומיין (Business, AgentConfig, CallRecord)
  db/                       pool, DataStore interface, postgres-store, migrate, seed
  http/
    routes.ts               כל ה-REST + שני ה-webhooks של Twilio
    schemas.ts              סכמות בקשה (כולל E.164)
    twilio-signature.ts     שומר חתימת Twilio (403 באי-התאמה)
    stream-token.ts         טוקן HMAC קצר-מועד ל-handshake של ה-WebSocket
    media-stream.ts         נקודת הקצה /webhooks/twilio/media
  realtime/
    protocol.ts             בוני הודעות לשני הצדדים + פרסינג בטוח
    openai-connection.ts    חיבור ל-OpenAI Realtime
    websocket-channel.ts    עטיפת ws ל-MessageChannel אחיד
    bridge.ts               MediaStreamBridge — הגישור עצמו
  future/interfaces.ts      תפרים ללא מימוש ל-Push 2+ (כלים, CRM, WhatsApp, קול)
migrations/                 001 סכמה ראשונית, 002 agent_configs + מזהי realtime
test/                       app.test.ts (API מול store בזיכרון), realtime.test.ts
```

עיצוב: מונוליט אחד, ללא תורים או מיקרו-שירותים. הגישה ל-DB מאחורי הממשק `DataStore`, מה שמאפשר לבדיקות להריץ את כל ה-API מול חנות בזיכרון.

### נתיב השיחה בזמן אמת

1. `POST /webhooks/twilio/voice` — חתימה מאומתת מול `TWILIO_AUTH_TOKEN` וה-URL המדויק שנבנה מ-`PUBLIC_BASE_URL` + נתיב הבקשה.
2. פתרון עסק לפי `To`, `upsertCall`, טעינת `agent_configs`.
3. סוכן פעיל → הנפקת טוקן HMAC הקשור ל-`CallSid` + `businessId`, והחזרת `<Connect><Stream>` ל-`wss://…/webhooks/twilio/media`. הטוקן נשלח כ-`<Parameter>` כי Twilio משמיט query strings מכתובות Stream.
4. ה-handshake נדחה בלי טוקן תקף; גם `CallSid` שבאירוע `start` חייב להתאים לטוקן (Twilio לא חותם על ה-WebSocket).
5. `MediaStreamBridge` מצמיד Media Stream אחד לסשן OpenAI Realtime אחד ומעביר אודיו G.711 mu-law (`audio/pcmu`) לשני הכיוונים **ללא טרנסקודינג**.
6. VAD בצד השרת + barge-in: כשהמתקשר מתחיל לדבר, הגשר שולח `truncate` ל-OpenAI ו-`clear` ל-Twilio, ומאפס את ה-marks הממתינים.
7. ברכה נאמרת ע"י ה-AI מיד עם `session.created`; `StreamSid` ו-`openai_session_id` נשמרים על רשומת השיחה.

## מודל הנתונים

- **businesses** — `id`, `name`, `phone_number` (ייחודי, CHECK ל-E.164), `greeting`, `active`, חותמות זמן.
- **agent_configs** — מפתח ראשי `business_id` (CASCADE): `instructions` (עד 8000 תווים), `greeting`, `language` (ברירת מחדל `he-IL`), `voice` (עד 80 תווים, ריק = הקול שכבר מוגדר אצל הספק), `realtime_model`, `voice_provider` (`openai` / `elevenlabs` / `cartesia`), `enabled`.
- **admin_users** — משתמשי הדשבורד: `email` ייחודי, `name`, `password_hash` (scrypt עם salt לכל hash), `active`, `last_login_at`.
- **admin_sessions** — סשנים בצד השרת: רק ה-SHA-256 של העוגייה נשמר, לצד `csrf_token` ו-`expires_at`.
- **audit_events** — היסטוריית שינויים ניהולית: מבצע הפעולה, `action`, סוג ומזהה הישות, תקציר, ו-`details` (jsonb) עם דיף ברמת שדה.
- **calls** — `twilio_call_sid` ייחודי, `from_number`/`to_number` עם CHECK ל-E.164, `status`, `direction`, `duration_seconds`, `twilio_stream_sid`, `openai_session_id`, זמני התחלה/סיום. מפתח זר ל-`businesses` עם `ON DELETE RESTRICT` — לכן מחיקת עסק עם היסטוריית שיחות מוחזרת כ-409.
- אינדקסים: `(business_id, created_at DESC)` ו-`(to_number, created_at DESC)`.

ה-seed יוצר עסק דמו `Callora Demo Business` עם המספר `+15551234567` וסוכן שירות לקוחות בעברית.

## API

| Method | Path | תיאור |
| --- | --- | --- |
| `GET` | `/health` | בריאות שירות + DB (503 כשה-DB נופל). ללא אימות |
| `GET` | `/dashboard/*` | הדשבורד הניהולי (עוגיית סשן) |
| `GET` | `/api/businesses` | רשימת עסקים |
| `POST` | `/api/businesses` | יצירת עסק |
| `GET` | `/api/businesses/:id` | קריאת עסק |
| `PATCH` | `/api/businesses/:id` | עדכון עסק |
| `DELETE` | `/api/businesses/:id` | מחיקה רק ללא היסטוריית שיחות (אחרת 409) |
| `GET` | `/api/calls?businessId=&limit=&offset=` | רשימת שיחות (עד 100 בעמוד) |
| `GET` | `/api/businesses/:id/agent` | קריאת קונפיגורציית הסוכן |
| `PUT` | `/api/businesses/:id/agent` | כתיבת קונפיגורציית הסוכן (422 אם הספק הנבחר אינו מוגדר בפלטפורמה) |
| `GET` | `/api/providers` | זמינות הספקים, בלי אף קרדנציאל |
| `GET` | `/api/calls/:id` | קריאת שיחה |
| `GET` | `/api/audit` | היסטוריית שינויים ניהולית |
| `GET` | `/api/me` | מי המבצע המאומת |
| `POST` | `/webhooks/twilio/voice` | webhook שיחה נכנסת (חתום) → TwiML |
| `POST` | `/webhooks/twilio/call-status` | קולבק מחזור-חיי שיחה (חתום) → 204 |
| `WS` | `/webhooks/twilio/media` | Media Stream דו-כיווני, מאומת בטוקן |

טיפול שגיאות מרכזי ממפה `23505` → 409 (כפילות), `23503` → 409 (הפניה קיימת), 4xx של Fastify כמו שהם, וכל השאר ל-500 גנרי עם לוג.

## קונפיגורציה

משתני סביבה מאומתים ב-`src/config.ts` (נכשל מהר בעליית התהליך):

חובה — `DATABASE_URL`, `TWILIO_AUTH_TOKEN`, `TWILIO_ACCOUNT_SID`, `PUBLIC_BASE_URL` (URL, נחתך ה-slash הסופי), והקרדנציאלים של ספק ברירת המחדל (`VOICE_PROVIDER`).
בקרת הפלטפורמה — `ADMIN_EMAIL` + `ADMIN_PASSWORD` יוצרים את מנהל הדשבורד ומאפסים את סיסמתו כשהערך משתנה, `ADMIN_API_KEY` הוא קרדנציאל מכונה אופציונלי ל-`/api`, ו-`SESSION_TTL_HOURS` קובע את אורך הסשן.
`VOICE_PROVIDER` הוא רק ברירת המחדל לסוכנים חדשים — כל עסק בוחר ספק משלו בדשבורד, וכל ספק שהקרדנציאלים שלו קיימים הופך לזמין לבחירה.
אופציונלי — `NODE_ENV`, `HOST` (`0.0.0.0`), `PORT` (`3000`), `LOG_LEVEL` (`info`), `TWILIO_ACCOUNT_SID`, `OPENAI_REALTIME_URL` (`wss://api.openai.com/v1/realtime`).

`PUBLIC_BASE_URL` חייב להיות זהה בדיוק לכתובת שמוגדרת ב-Twilio (כולל HTTPS וכל prefix), אחרת אימות החתימה יחזיר 403.

## פריסה

- אימג' backend רב-שלבי, Alpine, non-root, ל-`linux/arm64`.
- Compose לפרודקשן: backend + PostgreSQL + Caddy כ-reverse proxy עם HTTPS; volumes קבועים ו-healthchecks.
- GitHub Actions: מאמת PR-ים; דחיפה ל-`main` מפרסמת אימג'ים בתגי SHA ו-`latest` ל-Docker Hub פרטי ופורסת את ה-SHA האימוטבילי דרך SSH.
- יעד: Oracle Linux 9 ARM64 (`deploy/bootstrap-oracle-linux.sh`, `deploy/deploy.sh`).
- מיגרציות מוגנות ב-advisory lock; פריסה כושלת משחזרת את הקונפיגורציה/אימג' הקודמים בלי למחוק נתונים.

פרטים מלאים ב-[DEPLOYMENT.md](DEPLOYMENT.md).

## פקודות

```bash
pnpm install && pnpm db:migrate && pnpm db:seed && pnpm dev
```

```bash
docker compose up --build
```

`pnpm lint` · `pnpm build` · `pnpm test` · `pnpm validate:config` · `pnpm start`

## מה עוד לא קיים (מכוון)

MCP, Google Calendar, מחברי CRM, WhatsApp, חיוב, ידע/RAG, חשבונות ללקוחות העסק, ומערכת הכלים המלאה. `src/future/interfaces.ts` מגדיר את הממשקים לכל אלה ללא מימוש.

**מגבלה ידועה:** מודל ההרשאות שטוח — כל מנהל הוא מנהל פלטפורמה, ואין עדיין הרשאות ברמת דייר.

## אבני דרך הבאות

1. ניהול מאומת עם הרשאות מוגבלות-דייר.
2. שמירת תמלולי שיחות + טיפול ב-reconnect/דגרדציה בנתיב ה-realtime.
3. זרימת tool-call אחת ממוקדת ומדומה מעל קונפיגורציית הסוכן הקיימת.
4. חיבור כלים עסקיים אמיתיים; WhatsApp ושכפול קול נשארים אבני דרך נפרדות.
