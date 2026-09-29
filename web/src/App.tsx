import { useEffect, useState, type ReactNode } from "react";
import { BrowserRouter, Navigate, NavLink, Route, Routes } from "react-router-dom";
import { ClipboardList, LayoutDashboard, LogOut, Menu, MessageCircle, Moon, Phone, Settings, Sun, X } from "lucide-react";
import { api, type Session } from "./api";
import { cx, Loading } from "./ui";
import { Login } from "./pages/Login";
import { Overview } from "./pages/Overview";
import { Calls } from "./pages/Calls";
import { CallView } from "./pages/CallView";
import { Orders } from "./pages/Orders";
import { WhatsApp } from "./pages/WhatsApp";
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
    <BrowserRouter>
      <Shell business={auth.session.businesses[0]?.name ?? "קלורה"} onLogout={logout}>
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/calls" element={<Calls />} />
          <Route path="/calls/:id" element={<CallView />} />
          <Route path="/orders" element={<Orders />} />
          <Route path="/whatsapp" element={<WhatsApp />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </Shell>
    </BrowserRouter>
  );
}

const NAV = [
  { to: "/", label: "סקירה", icon: LayoutDashboard, end: true },
  { to: "/calls", label: "שיחות", icon: Phone, end: false },
  { to: "/orders", label: "הזמנות", icon: ClipboardList, end: false },
  { to: "/whatsapp", label: "וואטסאפ", icon: MessageCircle, end: false },
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

function Shell({ business, onLogout, children }: { business: string; onLogout: () => void; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const [dark, toggleTheme] = useTheme();

  const nav = (
    <nav className="flex flex-1 flex-col gap-1 px-3" aria-label="ניווט">
      {NAV.map(({ to, label, icon: Icon, end }) => (
        <NavLink
          key={to}
          to={to}
          end={end}
          onClick={() => setOpen(false)}
          className={({ isActive }) =>
            cx(
              "flex items-center gap-3 rounded-lg px-3 py-2 text-sm font-medium transition-colors",
              isActive
                ? "bg-brand-700 text-white shadow-sm dark:bg-brand-600"
                : "text-slate-600 hover:bg-slate-100 hover:text-slate-900 dark:text-slate-300 dark:hover:bg-slate-800 dark:hover:text-white",
            )
          }
        >
          <Icon className="size-4" aria-hidden />
          {label}
        </NavLink>
      ))}
    </nav>
  );

  const brand = (
    <div className="flex items-center gap-3 px-6 py-5">
      <div className="flex size-9 items-center justify-center rounded-lg bg-brand-700 text-white shadow-sm dark:bg-brand-600">
        <Phone className="size-4" aria-hidden />
      </div>
      <div>
        <div className="text-base font-bold tracking-tight text-slate-900 dark:text-white">קלורה</div>
        <div className="text-xs text-slate-500 dark:text-slate-400">{business}</div>
      </div>
    </div>
  );

  const footer = (
    <div className="flex items-center gap-1 border-t border-slate-200 px-3 py-3 dark:border-slate-800">
      <button
        type="button"
        onClick={toggleTheme}
        className="flex flex-1 items-center gap-2 rounded-lg px-3 py-2 text-sm text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-slate-800"
      >
        {dark ? <Sun className="size-4" aria-hidden /> : <Moon className="size-4" aria-hidden />}
        {dark ? "מצב בהיר" : "מצב כהה"}
      </button>
      <button
        type="button"
        onClick={onLogout}
        aria-label="התנתקות"
        title="התנתקות"
        className="rounded-lg p-2 text-slate-500 hover:bg-slate-100 hover:text-slate-900 dark:text-slate-400 dark:hover:bg-slate-800 dark:hover:text-white"
      >
        <LogOut className="size-4" aria-hidden />
      </button>
    </div>
  );

  return (
    <div className="min-h-dvh">
      {/* Wide screens: a fixed menu on the right. */}
      <aside className="fixed inset-y-0 right-0 z-30 hidden w-64 flex-col border-l border-slate-200 bg-white lg:flex dark:border-slate-800 dark:bg-slate-900">
        {brand}
        {nav}
        {footer}
      </aside>

      {/* Phones: a top bar that opens the menu. */}
      <div className="sticky top-0 z-20 flex items-center justify-between border-b border-slate-200 bg-white/90 px-4 py-2 backdrop-blur lg:hidden dark:border-slate-800 dark:bg-slate-900/90">
        <div className="font-bold">קלורה</div>
        <button type="button" onClick={() => setOpen(true)} aria-label="תפריט" className="rounded-lg p-2 hover:bg-slate-100 dark:hover:bg-slate-800">
          <Menu className="size-5" aria-hidden />
        </button>
      </div>
      {open && (
        <div className="fixed inset-0 z-40 lg:hidden" role="dialog" aria-modal="true">
          <button type="button" className="absolute inset-0 bg-slate-950/40" aria-label="סגירה" onClick={() => setOpen(false)} />
          <aside className="absolute inset-y-0 right-0 flex w-72 flex-col bg-white shadow-xl dark:bg-slate-900">
            <div className="flex items-center justify-between pe-3">
              {brand}
              <button type="button" onClick={() => setOpen(false)} aria-label="סגירה" className="rounded-lg p-2 hover:bg-slate-100 dark:hover:bg-slate-800">
                <X className="size-5" aria-hidden />
              </button>
            </div>
            {nav}
            {footer}
          </aside>
        </div>
      )}

      <main className="lg:ms-64">
        <div className="mx-auto max-w-7xl px-4 py-6 sm:px-6 lg:px-10 lg:py-8">{children}</div>
      </main>
    </div>
  );
}
