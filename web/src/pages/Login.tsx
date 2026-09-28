import { useState, type FormEvent } from "react";
import { Lock, Phone } from "lucide-react";
import { Button } from "../ui";

export function Login({ onSignedIn }: { onSignedIn: () => void }) {
  const [password, setPassword] = useState("");
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
    <div className="flex min-h-dvh items-center justify-center bg-gradient-to-b from-slate-100 to-slate-200 px-4 dark:from-slate-950 dark:to-slate-900">
      <div className="w-full max-w-sm">
        <div className="mb-8 flex flex-col items-center gap-3">
          <div className="flex size-12 items-center justify-center rounded-xl bg-brand-700 text-white shadow-md">
            <Phone className="size-5" aria-hidden />
          </div>
          <div className="text-center">
            <h1 className="text-xl font-bold tracking-tight text-slate-900 dark:text-white">קלורה</h1>
            <p className="text-sm text-slate-500 dark:text-slate-400">ניהול המוקד</p>
          </div>
        </div>
        <form onSubmit={submit} className="rounded-xl border border-slate-200 bg-white p-6 shadow-sm dark:border-slate-800 dark:bg-slate-900">
          <label htmlFor="password" className="mb-1.5 block text-sm font-medium text-slate-700 dark:text-slate-200">
            סיסמה
          </label>
          <div className="relative">
            <Lock className="pointer-events-none absolute inset-y-0 start-3 my-auto size-4 text-slate-400" aria-hidden />
            <input
              id="password"
              type="password"
              autoComplete="current-password"
              autoFocus
              required
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              className="w-full rounded-lg border border-slate-300 bg-white py-2.5 ps-9 pe-3 text-sm outline-none transition focus:border-brand-500 focus:ring-2 focus:ring-brand-200 dark:border-slate-700 dark:bg-slate-950 dark:focus:ring-brand-900"
            />
          </div>
          {error && (
            <p role="alert" className="mt-3 text-sm text-rose-600 dark:text-rose-400">
              {error}
            </p>
          )}
          <Button type="submit" variant="primary" disabled={busy || !password} className="mt-5 w-full py-2.5">
            {busy ? "מתחבר…" : "כניסה"}
          </Button>
        </form>
      </div>
    </div>
  );
}
