// The dashboard's building blocks: one look for every page.

import type { ButtonHTMLAttributes, ReactNode } from "react";
import { LoaderCircle, TriangleAlert } from "lucide-react";
import { dateTime } from "./format";

export function cx(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}

export function Card({ title, action, children, className }: { title?: ReactNode; action?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={cx("rounded-xl border border-slate-200 bg-white shadow-sm dark:border-slate-800 dark:bg-slate-900", className)}>
      {(title || action) && (
        <header className="flex items-center justify-between gap-3 border-b border-slate-100 px-5 py-3.5 dark:border-slate-800">
          <h2 className="text-sm font-semibold text-slate-700 dark:text-slate-200">{title}</h2>
          {action}
        </header>
      )}
      <div className="p-5">{children}</div>
    </section>
  );
}

type Tone = "neutral" | "good" | "bad" | "warn" | "brand";

const TONES: Record<Tone, string> = {
  neutral: "bg-slate-100 text-slate-700 ring-slate-200 dark:bg-slate-800 dark:text-slate-300 dark:ring-slate-700",
  good: "bg-emerald-50 text-emerald-700 ring-emerald-200 dark:bg-emerald-950/60 dark:text-emerald-300 dark:ring-emerald-900",
  bad: "bg-rose-50 text-rose-700 ring-rose-200 dark:bg-rose-950/60 dark:text-rose-300 dark:ring-rose-900",
  warn: "bg-amber-50 text-amber-800 ring-amber-200 dark:bg-amber-950/60 dark:text-amber-300 dark:ring-amber-900",
  brand: "bg-brand-50 text-brand-700 ring-brand-200 dark:bg-brand-900/60 dark:text-brand-200 dark:ring-brand-800",
};

export function Badge({ tone = "neutral", children }: { tone?: Tone; children: ReactNode }) {
  return (
    <span className={cx("inline-flex items-center gap-1 whitespace-nowrap rounded-full px-2 py-0.5 text-xs font-medium ring-1 ring-inset", TONES[tone])}>
      {children}
    </span>
  );
}

type Variant = "primary" | "secondary" | "ghost" | "good" | "bad";

const VARIANTS: Record<Variant, string> = {
  primary: "bg-brand-700 text-white hover:bg-brand-800 disabled:bg-brand-400 dark:bg-brand-600 dark:hover:bg-brand-500",
  secondary:
    "border border-slate-300 bg-white text-slate-700 hover:bg-slate-50 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-200 dark:hover:bg-slate-800",
  ghost: "text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-slate-800",
  good: "border border-emerald-300 bg-white text-emerald-700 hover:bg-emerald-50 aria-pressed:bg-emerald-600 aria-pressed:text-white aria-pressed:border-emerald-600 dark:bg-slate-900 dark:text-emerald-300 dark:border-emerald-800",
  bad: "border border-rose-300 bg-white text-rose-700 hover:bg-rose-50 aria-pressed:bg-rose-600 aria-pressed:text-white aria-pressed:border-rose-600 dark:bg-slate-900 dark:text-rose-300 dark:border-rose-800",
};

export function Button({ variant = "secondary", className, ...props }: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant }) {
  return (
    <button
      type="button"
      {...props}
      className={cx(
        "inline-flex items-center justify-center gap-2 rounded-lg px-3.5 py-2 text-sm font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-60",
        VARIANTS[variant],
        className,
      )}
    />
  );
}

export function Stat({ label, value, sub, tone }: { label: string; value: ReactNode; sub?: ReactNode; tone?: "good" | "warn" | "bad" }) {
  const color =
    tone === "good" ? "text-emerald-700 dark:text-emerald-400" : tone === "warn" ? "text-amber-700 dark:text-amber-400" : tone === "bad" ? "text-rose-700 dark:text-rose-400" : "text-slate-900 dark:text-white";
  return (
    <div className="rounded-xl border border-slate-200 bg-white p-4 shadow-sm dark:border-slate-800 dark:bg-slate-900">
      <div className="text-xs font-medium text-slate-500 dark:text-slate-400">{label}</div>
      <div className={cx("mt-1.5 text-2xl font-semibold tabular-nums tracking-tight", color)}>{value}</div>
      {sub && <div className="mt-1 text-xs text-slate-500 dark:text-slate-400">{sub}</div>}
    </div>
  );
}

export function Segmented<T extends string | number>({ value, options, onChange, label }: { value: T; options: { value: T; label: string }[]; onChange: (v: T) => void; label: string }) {
  return (
    <div role="group" aria-label={label} className="inline-flex rounded-lg border border-slate-300 bg-white p-0.5 dark:border-slate-700 dark:bg-slate-900">
      {options.map((o) => (
        <button
          key={String(o.value)}
          type="button"
          aria-pressed={o.value === value}
          onClick={() => onChange(o.value)}
          className="rounded-md px-3 py-1.5 text-sm text-slate-600 transition-colors hover:text-slate-900 aria-pressed:bg-brand-700 aria-pressed:text-white dark:text-slate-300 dark:hover:text-white dark:aria-pressed:bg-brand-600"
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function PageHeader({ title, subtitle, action }: { title: string; subtitle?: ReactNode; action?: ReactNode }) {
  return (
    <div className="mb-6 flex flex-wrap items-end justify-between gap-4">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight text-slate-900 dark:text-white">{title}</h1>
        {subtitle && <p className="mt-1 text-sm text-slate-500 dark:text-slate-400">{subtitle}</p>}
      </div>
      {action}
    </div>
  );
}

export function Loading({ label = "טוען…" }: { label?: string }) {
  return (
    <div className="flex items-center justify-center gap-2 py-12 text-sm text-slate-500">
      <LoaderCircle className="size-4 animate-spin" aria-hidden />
      {label}
    </div>
  );
}

export function Problem({ children }: { children: ReactNode }) {
  return (
    <div role="alert" className="flex items-center gap-2 rounded-lg border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-800 dark:border-rose-900 dark:bg-rose-950/50 dark:text-rose-300">
      <TriangleAlert className="size-4 shrink-0" aria-hidden />
      {children}
    </div>
  );
}

/** A date and time, read left to right inside Hebrew ("29.9.2026, 14:36"). */
export function When({ iso, className }: { iso: string; className?: string }) {
  return (
    <time dateTime={iso} className={cx("ltr inline-block tabular-nums", className)}>
      {dateTime(iso)}
    </time>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="py-12 text-center text-sm text-slate-500 dark:text-slate-400">{children}</div>;
}
