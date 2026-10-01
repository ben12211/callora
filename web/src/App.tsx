import { useEffect, useState, type ReactNode } from "react";
import { BrowserRouter, Navigate, NavLink, Route, Routes, useLocation } from "react-router-dom";
import { ClipboardList, LayoutDashboard, LogOut, MessageCircle, Moon, Phone, ScrollText, Settings, Sun } from "lucide-react";
import { api, type Session, type Stats, useApi } from "./api";
import { cx, Loading, ToastProvider } from "./ui";
import { Login } from "./pages/Login";
import { Overview } from "./pages/Overview";
import { Calls } from "./pages/Calls";
import { CallView } from "./pages/CallView";
import { Orders } from "./pages/Orders";
import { type WaOverview, WhatsApp, whatsappTrouble } from "./pages/WhatsApp";
import { Rules } from "./pages/Rules";
import { Settings as SettingsPage } from "./pages/Settings";

type Auth = { state: "checking" } | { state: "out" } | { state: "in"; session: Session };

export function App() {
  const [auth, setAuth] = useState<Auth>({ state: "checking" });

  const check = () =>
    api<Session>("/api/session")
      .then((session) => setAuth({ state: "in", session }))
      .catch(() => setAuth({ state: "out" }));

  useEffect(() => {
    void check();
    const out = () => setAuth({ state: "out" });
    window.addEventListener("callora:unauthorized", out);
    return () => window.removeEventListener("callora:unauthorized", out);
  }, []);

  if (auth.state === "checking") return <Loading />;
  if (auth.state === "out") return <Login onSignedIn={check} />;

  const logout = async () => {
    await api("/api/logout", { method: "POST" }).catch(() => undefined);
    setAuth({ state: "out" });
  };

  return (
    <ToastProvider>
      <BrowserRouter>
        <Shell session={auth.session} onLogout={logout}>
          <Routes>
            <Route path="/" element={<Overview session={auth.session} />} />
            <Route path="/calls" element={<Calls />} />
            <Route path="/calls/:id" element={<CallView />} />
            <Route path="/orders" element={<Orders />} />
            <Route path="/whatsapp" element={<WhatsApp />} />
            <Route path="/rules" element={<Rules />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </Shell>
      </BrowserRouter>
    </ToastProvider>
  );
}

type NavBadge = "verify" | "whatsapp";

const NAV: { to: string; label: string; icon: typeof Phone; end: boolean; badge?: NavBadge }[] = [
  { to: "/", label: "סקירה", icon: LayoutDashboard, end: true },
  { to: "/calls", label: "שיחות", icon: Phone, end: false },
  { to: "/orders", label: "הזמנות", icon: ClipboardList, end: false, badge: "verify" },
  { to: "/whatsapp", label: "חשבונות", icon: MessageCircle, end: false, badge: "whatsapp" },
  { to: "/rules", label: "כללי שיחה", icon: ScrollText, end: false },
  { to: "/settings", label: "הגדרות", icon: Settings, end: false },
];

function useTheme(): [boolean, () => void] {
  const [dark, setDark] = useState(() => {
    try {
      const saved = localStorage.getItem("callora_theme");
      if (saved) return saved === "dark";
    } catch {
      /* private mode */
    }
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  });
  useEffect(() => {
    document.documentElement.classList.toggle("dark", dark);
    document.querySelector('meta[name="theme-color"]')?.setAttribute("content", dark ? "#080c18" : "#f8fafc");
  }, [dark]);
  const toggle = () =>
    setDark((d) => {
      try {
        localStorage.setItem("callora_theme", d ? "light" : "dark");
      } catch {
        /* private mode */
      }
      return !d;
    });
  return [dark, toggle];
}

/** The mark: a phone on a gradient, with a live dot. */
export function Logo({ size = "md" }: { size?: "md" | "lg" }) {
  return (
    <div
      className={cx(
        "relative flex shrink-0 items-center justify-center bg-gradient-to-br from-brand-400 via-brand-600 to-brand-800 text-white shadow-lg shadow-brand-600/30 ring-1 ring-white/20",
        size === "lg" ? "size-14 rounded-2xl" : "size-10 rounded-xl",
      )}
    >
      <Phone className={size === "lg" ? "size-6" : "size-[18px]"} aria-hidden strokeWidth={2.2} />
      <span className="absolute -end-0.5 -top-0.5 size-2.5 rounded-full bg-emerald-400 ring-2 ring-white dark:ring-[#080c18]" aria-hidden />
    </div>
  );
}

function Shell({ session, onLogout, children }: { session: Session; onLogout: () => void; children: ReactNode }) {
  const [dark, toggleTheme] = useTheme();
  const stats = useApi<Stats>("/api/stats?days=30", 60_000);
  const wa = useApi<WaOverview>("/api/whatsapp", 60_000);
  const verify = stats.data?.to_verify ?? 0;
  const trouble = whatsappTrouble(wa.data) != null;
  const business = session.businesses[0]?.name ?? "קלורה";

  // A new page starts at the top, and the tab says where it is.
  const { pathname } = useLocation();
  useEffect(() => {
    window.scrollTo({ top: 0 });
    const page = NAV.find((n) => (n.end ? pathname === n.to : pathname.startsWith(n.to)));
    document.title = page && page.to !== "/" ? `${page.label} · קלורה` : "קלורה · ניהול";
  }, [pathname]);

  const badge = (b?: NavBadge): ReactNode => {
    if (b === "verify" && verify > 0) {
      return <span className="num flex h-5 min-w-5 items-center justify-center rounded-full bg-amber-500 px-1.5 text-[11px] font-bold text-white">{verify}</span>;
    }
    if (b === "whatsapp" && trouble) return <span className="size-2.5 rounded-full bg-rose-500 ring-2 ring-white dark:ring-slate-900" aria-label="דורש תשומת לב" />;
    return null;
  };

  return (
    <div className="min-h-dvh">
      {/* Wide screens: a fixed menu on the right. */}
      <aside className="fixed inset-y-0 right-0 z-30 hidden w-64 flex-col border-l border-slate-200/70 bg-white/80 backdrop-blur-xl lg:flex dark:border-white/[0.07] dark:bg-slate-950/60">
        <div className="flex items-center gap-3 px-5 pb-6 pt-6">
          <Logo />
          <div className="min-w-0">
            <div className="text-lg font-extrabold leading-none tracking-tight">קלורה</div>
            <div className="mt-1 truncate text-xs text-slate-500 dark:text-slate-400">{business}</div>
          </div>
        </div>

        <nav className="flex flex-1 flex-col gap-1 px-3" aria-label="ניווט">
          {NAV.map(({ to, label, icon: Icon, end, badge: b }) => (
            <NavLink
              key={to}
              to={to}
              end={end}
              className={({ isActive }) =>
                cx(
                  "group flex items-center gap-3 rounded-xl px-3.5 py-2.5 text-sm font-medium transition-colors",
                  isActive
                    ? "bg-brand-50 text-brand-700 dark:bg-brand-400/15 dark:text-brand-100"
                    : "text-slate-600 hover:bg-slate-100 hover:text-slate-900 dark:text-slate-400 dark:hover:bg-white/[0.06] dark:hover:text-white",
                )
              }
            >
              {({ isActive }) => (
                <>
                  <Icon className={cx("size-[18px] transition-colors", isActive ? "text-brand-600 dark:text-brand-300" : "text-slate-400 group-hover:text-slate-600 dark:text-slate-500 dark:group-hover:text-slate-300")} aria-hidden />
                  <span className="flex-1">{label}</span>
                  {badge(b)}
                </>
              )}
            </NavLink>
          ))}
        </nav>

        <div className="m-3 rounded-xl bg-slate-100/80 p-3 dark:bg-white/[0.05]">
          <div className="flex items-center gap-2 text-xs font-medium text-slate-700 dark:text-slate-200">
            <span className="relative flex size-2.5">
              <span className={cx("absolute inline-flex size-full animate-ping-slow rounded-full opacity-60", session.database ? "bg-emerald-400" : "bg-amber-400")} />
              <span className={cx("relative inline-flex size-2.5 rounded-full", session.database ? "bg-emerald-500" : "bg-amber-500")} />
            </span>
            {session.database ? "המערכת פעילה" : "אין חיבור למסד הנתונים"}
          </div>
          <div className="mt-2.5 flex items-center gap-1">
            <button
              type="button"
              onClick={toggleTheme}
              className="flex flex-1 items-center gap-2 rounded-lg px-2 py-1.5 text-xs text-slate-600 transition-colors hover:bg-white dark:text-slate-300 dark:hover:bg-white/10"
            >
              {dark ? <Sun className="size-3.5" aria-hidden /> : <Moon className="size-3.5" aria-hidden />}
              {dark ? "מצב בהיר" : "מצב כהה"}
            </button>
            <button
              type="button"
              onClick={onLogout}
              aria-label="התנתקות"
              title="התנתקות"
              className="rounded-lg p-1.5 text-slate-500 transition-colors hover:bg-white hover:text-slate-900 dark:text-slate-400 dark:hover:bg-white/10 dark:hover:text-white"
            >
              <LogOut className="size-3.5" aria-hidden />
            </button>
          </div>
        </div>
      </aside>

      {/* Phones: a slim bar on top, and the menu where the thumb is. */}
      <header className="sticky top-0 z-20 flex items-center justify-between border-b border-slate-200/70 bg-white/80 px-4 py-2.5 backdrop-blur-xl lg:hidden dark:border-white/[0.07] dark:bg-slate-950/70">
        <div className="flex items-center gap-2.5">
          <Logo />
          <div className="leading-tight">
            <div className="font-extrabold tracking-tight">קלורה</div>
            <div className="text-[11px] text-slate-500 dark:text-slate-400">{business}</div>
          </div>
        </div>
        <div className="flex items-center gap-1">
          <button type="button" onClick={toggleTheme} aria-label={dark ? "מצב בהיר" : "מצב כהה"} className="rounded-xl p-2.5 text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-white/10">
            {dark ? <Sun className="size-[18px]" aria-hidden /> : <Moon className="size-[18px]" aria-hidden />}
          </button>
          <button type="button" onClick={onLogout} aria-label="התנתקות" className="rounded-xl p-2.5 text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-white/10">
            <LogOut className="size-[18px]" aria-hidden />
          </button>
        </div>
      </header>

      <nav
        aria-label="ניווט"
        className="fixed inset-x-0 bottom-0 z-30 grid grid-cols-6 border-t border-slate-200/70 bg-white/90 px-1 pt-1.5 backdrop-blur-xl lg:hidden dark:border-white/[0.07] dark:bg-slate-950/85"
        style={{ paddingBottom: "max(0.375rem, env(safe-area-inset-bottom))" }}
      >
        {NAV.map(({ to, label, icon: Icon, end, badge: b }) => (
          <NavLink key={to} to={to} end={end} className="group flex flex-col items-center gap-0.5 py-1 text-[11px] font-medium">
            {({ isActive }) => (
              <>
                <span
                  className={cx(
                    "relative flex h-8 w-14 items-center justify-center rounded-full transition-colors",
                    isActive ? "bg-brand-100 text-brand-700 dark:bg-brand-400/20 dark:text-brand-100" : "text-slate-500 dark:text-slate-400",
                  )}
                >
                  <Icon className="size-5" aria-hidden />
                  <span className="absolute -top-0.5 end-2.5">{badge(b)}</span>
                </span>
                <span className={isActive ? "text-brand-700 dark:text-brand-100" : "text-slate-500 dark:text-slate-400"}>{label}</span>
              </>
            )}
          </NavLink>
        ))}
      </nav>

      <main className="lg:ms-64">
        <div className="pb-tabbar mx-auto max-w-[88rem] px-4 pt-5 sm:px-6 lg:px-10 lg:pb-12 lg:pt-9">{children}</div>
      </main>
    </div>
  );
}
