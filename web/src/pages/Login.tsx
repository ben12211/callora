import { useState, type FormEvent } from "react";
import { Eye, EyeOff, Headset, Lock, Mic, TrendingUp } from "lucide-react";
import { Logo } from "../App";
import { Button, FIELD, cx } from "../ui";

const POINTS = [
  { icon: Mic, title: "שומעים כל שיחה", text: "מה נאמר, מה הסוכן הבין, ובכמה זמן ענה." },
  { icon: Headset, title: "מוקדן כשצריך", text: "העברה לבן אדם עם סיכום, ומוזיקה בזמן ההמתנה." },
  { icon: TrendingUp, title: "רואים מה עובד", text: "כמה הזמנות נסגרו לבד, וכמה עלתה כל שיחה." },
];

export function Login({ onSignedIn }: { onSignedIn: () => void }) {
  const [password, setPassword] = useState("");
  const [show, setShow] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const res = await fetch("/api/login", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ password }),
        credentials: "same-origin",
      });
      if (res.ok) {
        onSignedIn();
        return;
      }
      setError(res.status === 429 ? "יותר מדי ניסיונות. אפשר לנסות שוב בעוד דקה." : "הסיסמה שגויה");
    } catch {
      setError("אין חיבור לשרת");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="grid min-h-dvh lg:grid-cols-[minmax(0,1fr)_minmax(0,1.1fr)]">
      <main className="flex items-center justify-center px-5 py-10">
        <div className="w-full max-w-sm animate-rise">
          <div className="mb-8 flex items-center gap-3.5">
            <Logo size="lg" />
            <div>
              <h1 className="text-2xl font-extrabold tracking-tight">קלורה</h1>
              <p className="text-sm text-slate-500 dark:text-slate-400">ניהול המוקד</p>
            </div>
          </div>

          <h2 className="text-xl font-bold tracking-tight">ברוכים השבים</h2>
          <p className="mb-6 mt-1 text-sm text-slate-500 dark:text-slate-400">הכניסו את הסיסמה כדי לראות מה קורה בקו.</p>

          <form onSubmit={submit} className="grid gap-4">
            <div className="grid gap-1.5">
              <label htmlFor="password" className="text-sm font-medium">
                סיסמה
              </label>
              <div className="relative">
                <Lock className="pointer-events-none absolute inset-y-0 start-3.5 my-auto size-4 text-slate-400" aria-hidden />
                <input
                  id="password"
                  type={show ? "text" : "password"}
                  autoComplete="current-password"
                  autoFocus
                  required
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  aria-invalid={error ? true : undefined}
                  aria-describedby={error ? "login-error" : undefined}
                  className={cx(FIELD, "ps-10 pe-11", error && "ring-2 ring-rose-400 dark:ring-rose-400/70")}
                />
                <button
                  type="button"
                  onClick={() => setShow((s) => !s)}
                  aria-label={show ? "הסתרת הסיסמה" : "הצגת הסיסמה"}
                  className="absolute inset-y-0 end-2 my-auto flex size-8 items-center justify-center rounded-lg text-slate-400 transition-colors hover:bg-slate-100 hover:text-slate-600 dark:hover:bg-white/10"
                >
                  {show ? <EyeOff className="size-4" aria-hidden /> : <Eye className="size-4" aria-hidden />}
                </button>
              </div>
              {error && (
                <p id="login-error" role="alert" className="text-sm font-medium text-rose-600 dark:text-rose-400">
                  {error}
                </p>
              )}
            </div>
            <Button type="submit" variant="primary" disabled={busy || !password} className="py-3 text-[15px]">
              {busy ? "מתחבר…" : "כניסה"}
            </Button>
          </form>
        </div>
      </main>

      <aside className="relative hidden overflow-hidden bg-gradient-to-br from-brand-600 via-brand-800 to-brand-950 p-14 text-white lg:flex lg:flex-col lg:justify-center">
        <div className="pointer-events-none absolute -end-24 -top-24 size-96 rounded-full bg-brand-400/30 blur-3xl" aria-hidden />
        <div className="pointer-events-none absolute -bottom-32 start-0 size-[28rem] rounded-full bg-emerald-400/15 blur-3xl" aria-hidden />
        <div
          className="pointer-events-none absolute inset-0 opacity-[0.07]"
          style={{ backgroundImage: "radial-gradient(circle at 1px 1px, white 1px, transparent 0)", backgroundSize: "26px 26px" }}
          aria-hidden
        />
        <div className="relative max-w-md">
          <h2 className="text-4xl font-extrabold leading-tight tracking-tight">מוקד שעונה לבד,
            <br />
            ואתם רואים הכול.</h2>
          <ul className="mt-10 space-y-6">
            {POINTS.map(({ icon: Icon, title, text }) => (
              <li key={title} className="flex gap-4">
                <span className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-white/12 ring-1 ring-white/20 backdrop-blur">
                  <Icon className="size-5" aria-hidden />
                </span>
                <div>
                  <div className="font-semibold">{title}</div>
                  <div className="mt-0.5 text-sm leading-relaxed text-white/70">{text}</div>
                </div>
              </li>
            ))}
          </ul>
        </div>
      </aside>
    </div>
  );
}
