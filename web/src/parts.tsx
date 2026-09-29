// Pieces several pages share: how a call ended, and what a ride looks like.

import type { ReactNode } from "react";
import { CarTaxiFront, CircleCheck, Clock, Headset, MapPin, PhoneOff, Radio } from "lucide-react";
import type { CallRow, Order } from "./api";
import { OUTCOMES } from "./format";
import { Badge, cx, type Tone } from "./ui";

export type CallState = { label: string; tone: Tone; icon: typeof Radio; live?: boolean };

/** What a call came to, in one word. A booked ride outranks how the line closed. */
export function callState(outcome: string | null, orders = 0): CallState {
  if (!outcome) return { label: "בשיחה", tone: "brand", icon: Radio, live: true };
  if (orders > 0) return { label: "הוזמנה מונית", tone: "good", icon: CarTaxiFront };
  switch (outcome) {
    case "HandedOff":
      return { label: "הועבר למוקדן", tone: "warn", icon: Headset };
    case "CallerHungUp":
      return { label: "המתקשר ניתק", tone: "neutral", icon: PhoneOff };
    case "TimeLimit":
      return { label: OUTCOMES.TimeLimit, tone: "neutral", icon: Clock };
    default:
      return { label: OUTCOMES[outcome] ?? outcome, tone: "neutral", icon: CircleCheck };
  }
}

const AVATAR: Record<Tone, string> = {
  good: "bg-emerald-100 text-emerald-700 dark:bg-emerald-400/15 dark:text-emerald-300",
  warn: "bg-amber-100 text-amber-700 dark:bg-amber-400/15 dark:text-amber-300",
  bad: "bg-rose-100 text-rose-700 dark:bg-rose-400/15 dark:text-rose-300",
  brand: "bg-brand-100 text-brand-700 dark:bg-brand-400/20 dark:text-brand-200",
  neutral: "bg-slate-100 text-slate-500 dark:bg-white/[0.07] dark:text-slate-400",
};

export function CallAvatar({ outcome, orders = 0, size = "md" }: { outcome: string | null; orders?: number; size?: "sm" | "md" | "lg" }) {
  const s = callState(outcome, orders);
  const Icon = s.icon;
  return (
    <span
      className={cx(
        "relative flex shrink-0 items-center justify-center rounded-full",
        AVATAR[s.tone],
        size === "lg" ? "size-14" : size === "sm" ? "size-8" : "size-10",
      )}
      title={s.label}
    >
      <Icon className={size === "lg" ? "size-6" : size === "sm" ? "size-4" : "size-[18px]"} aria-hidden />
      {s.live && <span className="absolute inset-0 animate-ping-slow rounded-full bg-brand-400/40" aria-hidden />}
    </span>
  );
}

export function OutcomeBadge({ outcome, orders = 0 }: { outcome: string | null; orders?: number }) {
  const s = callState(outcome, orders);
  return (
    <Badge tone={s.tone} dot>
      {s.label}
    </Badge>
  );
}

export function VerdictBadge({ verdict }: { verdict: CallRow["verdict"] }) {
  if (!verdict) return null;
  return <Badge tone={verdict === "good" ? "good" : "bad"}>{verdict === "good" ? "תקין" : "לא תקין"}</Badge>;
}

/** The value of one field of an order card. */
export function detail(order: Order, field: string): string | null {
  const d = order.card.details.find((x) => x.field === field);
  return d && d.value.trim() ? d.value : null;
}

/** Pickup and destination as a route: two stops on one line. */
export function Route({ from, to, compact }: { from: ReactNode; to: ReactNode; compact?: boolean }) {
  return (
    <div className="flex gap-3">
      <div className="flex flex-col items-center pt-1.5" aria-hidden>
        <span className="flex size-4 items-center justify-center rounded-full bg-emerald-100 dark:bg-emerald-400/20">
          <span className="size-2 rounded-full bg-emerald-500" />
        </span>
        <span className={cx("w-px flex-1 border-s-2 border-dotted border-slate-300 dark:border-white/20", compact ? "my-1 min-h-3" : "my-1.5 min-h-5")} />
        <MapPin className="size-4 text-rose-500" />
      </div>
      <div className={cx("flex min-w-0 flex-1 flex-col", compact ? "gap-2" : "gap-3.5")}>
        <div className="min-w-0">
          <div className="text-[11px] font-medium text-slate-500 dark:text-slate-400">מאיפה</div>
          <div className="break-words text-sm font-semibold leading-snug">{from}</div>
        </div>
        <div className="min-w-0">
          <div className="text-[11px] font-medium text-slate-500 dark:text-slate-400">לאן</div>
          <div className="break-words text-sm font-semibold leading-snug">{to}</div>
        </div>
      </div>
    </div>
  );
}

