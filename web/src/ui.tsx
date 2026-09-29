// The dashboard's building blocks: one look for every page.

import { createContext, useContext, useEffect, useId, useMemo, useState, type ButtonHTMLAttributes, type InputHTMLAttributes, type ReactNode } from "react";
import { ArrowDownRight, ArrowUpRight, Check, CircleAlert, LoaderCircle, Minus, TriangleAlert, X } from "lucide-react";
import { ago, dateTime } from "./format";

export function cx(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}

/** The surface everything sits on. */
export const SURFACE =
  "rounded-2xl bg-white shadow-card ring-1 ring-slate-200/70 dark:bg-slate-900/70 dark:ring-white/[0.08] dark:shadow-none";

export function Card({
  title,
  subtitle,
  action,
  children,
  className,
  flush,
}: {
  title?: ReactNode;
  subtitle?: ReactNode;
  action?: ReactNode;
  children: ReactNode;
  className?: string;
  /** No padding around the content (a table that runs to the edges). */
  flush?: boolean;
}) {
  return (
    <section className={cx(SURFACE, "animate-rise", className)}>
      {(title || action) && (
        <header className="flex items-start justify-between gap-3 px-5 pt-4 pb-1">
          <div className="min-w-0">
            <h2 className="text-[15px] font-semibold tracking-tight text-slate-900 dark:text-slate-50">{title}</h2>
            {subtitle && <p className="mt-0.5 text-xs text-slate-500 dark:text-slate-400">{subtitle}</p>}
          </div>
          {action}
        </header>
      )}
      <div className={flush ? "pt-3" : cx("p-5", Boolean(title || action) && "pt-3")}>{children}</div>
    </section>
  );
}

export type Tone = "neutral" | "good" | "bad" | "warn" | "brand";

const TONES: Record<Tone, string> = {
  neutral: "bg-slate-100 text-slate-700 ring-slate-200/80 dark:bg-white/[0.06] dark:text-slate-300 dark:ring-white/10",
  good: "bg-emerald-50 text-emerald-700 ring-emerald-200/80 dark:bg-emerald-400/10 dark:text-emerald-300 dark:ring-emerald-400/20",
  bad: "bg-rose-50 text-rose-700 ring-rose-200/80 dark:bg-rose-400/10 dark:text-rose-300 dark:ring-rose-400/20",
  warn: "bg-amber-50 text-amber-800 ring-amber-200/80 dark:bg-amber-400/10 dark:text-amber-300 dark:ring-amber-400/20",
  brand: "bg-brand-50 text-brand-700 ring-brand-200/80 dark:bg-brand-400/10 dark:text-brand-200 dark:ring-brand-400/20",
};

const DOTS: Record<Tone, string> = {
  neutral: "bg-slate-400",
  good: "bg-emerald-500",
  bad: "bg-rose-500",
  warn: "bg-amber-500",
  brand: "bg-brand-500",
};

export function Badge({ tone = "neutral", dot, children }: { tone?: Tone; dot?: boolean; children: ReactNode }) {
  return (
    <span className={cx("inline-flex items-center gap-1.5 whitespace-nowrap rounded-full px-2.5 py-0.5 text-xs font-medium ring-1 ring-inset", TONES[tone])}>
      {dot && <span className={cx("size-1.5 rounded-full", DOTS[tone])} aria-hidden />}
      {children}
    </span>
  );
}

type Variant = "primary" | "secondary" | "ghost" | "good" | "bad";

const VARIANTS: Record<Variant, string> = {
  primary:
    "bg-brand-600 text-white shadow-sm shadow-brand-600/25 hover:bg-brand-500 active:bg-brand-700 disabled:bg-brand-300 disabled:shadow-none dark:disabled:bg-brand-900",
  secondary:
    "bg-white text-slate-700 shadow-sm ring-1 ring-inset ring-slate-300/80 hover:bg-slate-50 dark:bg-white/[0.06] dark:text-slate-200 dark:ring-white/10 dark:hover:bg-white/10 dark:shadow-none",
  ghost: "text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-white/[0.07]",
  good: "bg-white text-emerald-700 ring-1 ring-inset ring-emerald-300 hover:bg-emerald-50 aria-pressed:bg-emerald-600 aria-pressed:text-white aria-pressed:ring-emerald-600 dark:bg-transparent dark:text-emerald-300 dark:ring-emerald-500/40 dark:hover:bg-emerald-500/10 dark:aria-pressed:bg-emerald-500 dark:aria-pressed:text-emerald-950",
  bad: "bg-white text-rose-700 ring-1 ring-inset ring-rose-300 hover:bg-rose-50 aria-pressed:bg-rose-600 aria-pressed:text-white aria-pressed:ring-rose-600 dark:bg-transparent dark:text-rose-300 dark:ring-rose-500/40 dark:hover:bg-rose-500/10 dark:aria-pressed:bg-rose-500 dark:aria-pressed:text-rose-950",
};

export function Button({ variant = "secondary", className, ...props }: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant }) {
  return (
    <button
      type="button"
      {...props}
      className={cx(
        "inline-flex items-center justify-center gap-2 rounded-xl px-4 py-2 text-sm font-medium transition-all duration-150 active:scale-[0.98] disabled:cursor-not-allowed disabled:opacity-60 disabled:active:scale-100",
        VARIANTS[variant],
        className,
      )}
    />
  );
}

/** One field style for every input. */
export const FIELD =
  "w-full rounded-xl border-0 bg-white px-3.5 py-2.5 text-sm text-slate-900 shadow-sm ring-1 ring-inset ring-slate-300/80 outline-none transition placeholder:text-slate-400 focus:ring-2 focus:ring-brand-500 dark:bg-white/[0.05] dark:text-slate-100 dark:ring-white/10 dark:placeholder:text-slate-500 dark:shadow-none dark:focus:ring-brand-400";

export function Input(props: InputHTMLAttributes<HTMLInputElement>) {
  return <input {...props} className={cx(FIELD, props.className)} />;
}

export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: (id: string) => ReactNode }) {
  const id = useId();
  return (
    <div className="grid gap-1.5">
      <label htmlFor={id} className="text-sm font-medium text-slate-800 dark:text-slate-100">
        {label}
      </label>
      {children(id)}
      {hint && <p className="text-xs leading-relaxed text-slate-500 dark:text-slate-400">{hint}</p>}
    </div>
  );
}

/** A small stat: a label, a number, a line of context. */
export function Stat({ label, value, sub, tone }: { label: string; value: ReactNode; sub?: ReactNode; tone?: "good" | "warn" | "bad" }) {
  const color =
    tone === "good" ? "text-emerald-600 dark:text-emerald-400" : tone === "warn" ? "text-amber-600 dark:text-amber-400" : tone === "bad" ? "text-rose-600 dark:text-rose-400" : "text-slate-900 dark:text-white";
  return (
    <div className={cx(SURFACE, "p-4")}>
      <div className="text-xs font-medium text-slate-500 dark:text-slate-400">{label}</div>
      <div className={cx("num mt-1.5 text-2xl font-bold tracking-tight", color)}>{value}</div>
      {sub && <div className="mt-1 text-xs text-slate-500 dark:text-slate-400">{sub}</div>}
    </div>
  );
}

export function Segmented<T extends string | number>({
  value,
  options,
  onChange,
  label,
}: {
  value: T;
  options: { value: T; label: string; count?: number }[];
  onChange: (v: T) => void;
  label: string;
}) {
  return (
    <div role="group" aria-label={label} className="inline-flex max-w-full gap-0.5 overflow-x-auto rounded-xl bg-slate-200/60 p-1 dark:bg-white/[0.06]">
      {options.map((o) => (
        <button
          key={String(o.value)}
          type="button"
          aria-pressed={o.value === value}
          onClick={() => onChange(o.value)}
          className="inline-flex items-center gap-1.5 whitespace-nowrap rounded-lg px-3 py-1.5 text-sm font-medium text-slate-600 transition-all hover:text-slate-900 aria-pressed:bg-white aria-pressed:text-slate-900 aria-pressed:shadow-sm dark:text-slate-400 dark:hover:text-white dark:aria-pressed:bg-white/[0.12] dark:aria-pressed:text-white dark:aria-pressed:shadow-none"
        >
          {o.label}
          {o.count != null && (
            <span className="num rounded-md bg-slate-300/50 px-1.5 text-[11px] leading-5 text-slate-600 dark:bg-white/10 dark:text-slate-300">{o.count}</span>
          )}
        </button>
      ))}
    </div>
  );
}

export function PageHeader({ title, subtitle, action }: { title: ReactNode; subtitle?: ReactNode; action?: ReactNode }) {
  return (
    <div className="mb-6 flex flex-wrap items-end justify-between gap-4">
      <div className="min-w-0">
        <h1 className="text-[1.65rem] font-extrabold leading-tight tracking-tight text-slate-900 sm:text-3xl dark:text-white">{title}</h1>
        {subtitle && <p className="mt-1 text-sm text-slate-500 dark:text-slate-400">{subtitle}</p>}
      </div>
      {action}
    </div>
  );
}

export function Loading({ label = "טוען…" }: { label?: string }) {
  return (
    <div className="flex items-center justify-center gap-2 py-16 text-sm text-slate-500 dark:text-slate-400">
      <LoaderCircle className="size-4 animate-spin" aria-hidden />
      {label}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div aria-hidden className={cx("animate-pulse rounded-lg bg-slate-200/80 dark:bg-white/[0.07]", className)} />;
}

export function Problem({ children }: { children: ReactNode }) {
  return (
    <div role="alert" className="flex items-center gap-2.5 rounded-xl bg-rose-50 px-4 py-3 text-sm text-rose-800 ring-1 ring-inset ring-rose-200 dark:bg-rose-500/10 dark:text-rose-200 dark:ring-rose-400/20">
      <TriangleAlert className="size-4 shrink-0" aria-hidden />
      {children}
    </div>
  );
}

/** A date and time, read left to right inside Hebrew ("29.9.2026, 14:36"). */
export function When({ iso, className }: { iso: string; className?: string }) {
  return (
    <time dateTime={iso} className={cx("ltr num inline-block", className)}>
      {dateTime(iso)}
    </time>
  );
}

/** "לפני 5 דקות", kept fresh, with the full date on hover. */
export function Ago({ iso, className }: { iso: string; className?: string }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(t);
  }, []);
  return (
    <time dateTime={iso} title={dateTime(iso)} className={className}>
      {ago(iso, now)}
    </time>
  );
}

export function Empty({ children, icon, action }: { children: ReactNode; icon?: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-3 py-14 text-center text-sm text-slate-500 dark:text-slate-400">
      {icon && <div className="flex size-12 items-center justify-center rounded-2xl bg-slate-100 text-slate-400 dark:bg-white/[0.06] dark:text-slate-500">{icon}</div>}
      <div>{children}</div>
      {action}
    </div>
  );
}

/** How much a number moved since the period before. */
export function Delta({ value, goodWhen = "up" }: { value: number | null; goodWhen?: "up" | "down" | "none" }) {
  if (value == null) return <span className="text-xs text-slate-400">אין להשוות</span>;
  const rounded = Math.round(value * 100);
  if (rounded === 0) {
    return (
      <span className="inline-flex items-center gap-0.5 text-xs font-medium text-slate-500">
        <Minus className="size-3" aria-hidden />
        ללא שינוי
      </span>
    );
  }
  const up = rounded > 0;
  const good = goodWhen === "none" ? null : goodWhen === "up" ? up : !up;
  const color = good == null ? "text-slate-500" : good ? "text-emerald-600 dark:text-emerald-400" : "text-rose-600 dark:text-rose-400";
  const Icon = up ? ArrowUpRight : ArrowDownRight;
  return (
    <span className={cx("num inline-flex items-center gap-0.5 text-xs font-semibold", color)}>
      <Icon className="size-3.5" aria-hidden />
      {Math.abs(rounded)}%
      <span className="sr-only">{up ? "עלייה" : "ירידה"}</span>
    </span>
  );
}

/** A tiny trend line. `data` is oldest first; time runs right to left, as in Hebrew. */
export function Sparkline({ data, color = "#6b66f2", className }: { data: number[]; color?: string; className?: string }) {
  const id = useId();
  const path = useMemo(() => {
    const w = 120;
    const h = 36;
    const max = Math.max(...data, 1);
    const step = data.length > 1 ? w / (data.length - 1) : 0;
    const pts = data.map((v, i) => [w - i * step, h - 3 - (v / max) * (h - 8)] as const);
    const line = pts.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(1)} ${y.toFixed(1)}`).join(" ");
    const [firstX] = pts[0] ?? [w];
    const [lastX] = pts[pts.length - 1] ?? [0];
    return { line, area: `${line} L${lastX.toFixed(1)} ${h} L${firstX.toFixed(1)} ${h} Z`, last: pts[0] };
  }, [data]);
  if (data.length < 2) return null;
  return (
    <svg viewBox="0 0 120 36" preserveAspectRatio="none" className={cx("h-9 w-full overflow-visible", className)} aria-hidden>
      <defs>
        <linearGradient id={id} x1="0" x2="0" y1="0" y2="1">
          <stop offset="0" stopColor={color} stopOpacity="0.28" />
          <stop offset="1" stopColor={color} stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={path.area} fill={`url(#${id})`} />
      <path d={path.line} fill="none" stroke={color} strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

/** A ring with a share filled in, and something in the middle. */
export function Ring({ share, color = "#10b981", size = 76, stroke = 9, children }: { share: number; color?: string; size?: number; stroke?: number; children?: ReactNode }) {
  const r = (size - stroke) / 2;
  const c = 2 * Math.PI * r;
  const s = Math.min(Math.max(share, 0), 1);
  return (
    <div className="relative shrink-0" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90 scale-x-[-1]" aria-hidden>
        <circle cx={size / 2} cy={size / 2} r={r} fill="none" strokeWidth={stroke} className="stroke-slate-200 dark:stroke-white/10" />
        <circle
          cx={size / 2}
          cy={size / 2}
          r={r}
          fill="none"
          stroke={color}
          strokeWidth={stroke}
          strokeLinecap="round"
          strokeDasharray={`${c * s} ${c}`}
          style={{ transition: "stroke-dasharray 0.8s cubic-bezier(0.22, 1, 0.36, 1)" }}
        />
      </svg>
      <div className="absolute inset-0 flex items-center justify-center">{children}</div>
    </div>
  );
}

/** A donut split into parts. */
export function Donut({ parts, size = 148, stroke = 18, children }: { parts: { value: number; color: string }[]; size?: number; stroke?: number; children?: ReactNode }) {
  const total = parts.reduce((a, p) => a + p.value, 0);
  const r = (size - stroke) / 2;
  const c = 2 * Math.PI * r;
  let offset = 0;
  return (
    <div className="relative shrink-0" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90 scale-x-[-1]" aria-hidden>
        <circle cx={size / 2} cy={size / 2} r={r} fill="none" strokeWidth={stroke} className="stroke-slate-200 dark:stroke-white/10" />
        {total > 0 &&
          parts.map((p, i) => {
            const len = (p.value / total) * c;
            const gap = parts.filter((q) => q.value > 0).length > 1 ? 2.5 : 0;
            const el = (
              <circle
                key={i}
                cx={size / 2}
                cy={size / 2}
                r={r}
                fill="none"
                stroke={p.color}
                strokeWidth={stroke}
                strokeDasharray={`${Math.max(len - gap, 0)} ${c}`}
                strokeDashoffset={-offset}
                strokeLinecap="butt"
              />
            );
            offset += len;
            return el;
          })}
      </svg>
      <div className="absolute inset-0 flex flex-col items-center justify-center text-center">{children}</div>
    </div>
  );
}

// ---------------------------------------------------------------------------------------
// A message that says something happened, then goes away.

type ToastTone = "good" | "bad" | "neutral";
type Push = (message: string, tone?: ToastTone) => void;
const ToastContext = createContext<Push>(() => undefined);

export function useToast(): Push {
  return useContext(ToastContext);
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<{ id: number; message: string; tone: ToastTone }[]>([]);
  const push: Push = (message, tone = "neutral") => {
    const id = Date.now() + Math.random();
    setItems((xs) => [...xs.slice(-2), { id, message, tone }]);
    window.setTimeout(() => setItems((xs) => xs.filter((x) => x.id !== id)), 3800);
  };
  return (
    <ToastContext.Provider value={push}>
      {children}
      <div aria-live="polite" className="pointer-events-none fixed inset-x-0 bottom-24 z-50 flex flex-col items-center gap-2 px-4 lg:bottom-6">
        {items.map((t) => (
          <div
            key={t.id}
            role="status"
            className="pointer-events-auto flex animate-rise items-center gap-2.5 rounded-xl bg-slate-900 px-4 py-2.5 text-sm font-medium text-white shadow-pop ring-1 ring-white/10 dark:bg-slate-100 dark:text-slate-900"
          >
            {t.tone === "good" ? <Check className="size-4 text-emerald-400 dark:text-emerald-600" aria-hidden /> : t.tone === "bad" ? <CircleAlert className="size-4 text-rose-400 dark:text-rose-600" aria-hidden /> : null}
            {t.message}
            <button type="button" aria-label="סגירה" className="-me-1 rounded p-0.5 opacity-60 hover:opacity-100" onClick={() => setItems((xs) => xs.filter((x) => x.id !== t.id))}>
              <X className="size-3.5" aria-hidden />
            </button>
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}
