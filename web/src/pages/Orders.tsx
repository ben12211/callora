import { useMemo } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { ArrowLeft, CarTaxiFront, Clock, Phone, StickyNote, TriangleAlert, User, Users } from "lucide-react";
import { type Order, useApi } from "../api";
import { hhmm, phone } from "../format";
import { Badge, Empty, PageHeader, Problem, Segmented, Skeleton, SURFACE, cx } from "../ui";
import { Route, detail } from "../parts";

type Filter = "all" | "verify";

const TZ = "Asia/Jerusalem";
const dayKey = (iso: string) => new Date(iso).toLocaleDateString("en-CA", { timeZone: TZ });

function dayTitle(key: string): string {
  const now = Date.now();
  if (key === dayKey(new Date(now).toISOString())) return "היום";
  if (key === dayKey(new Date(now - 86_400_000).toISOString())) return "אתמול";
  const [y, m, d] = key.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d, 12)).toLocaleDateString("he-IL", { timeZone: TZ, weekday: "long", day: "numeric", month: "long" });
}

export function Orders() {
  const [params, setParams] = useSearchParams();
  const filter: Filter = params.get("verify") ? "verify" : "all";
  const orders = useApi<Order[]>("/api/orders", 30_000);

  const verifyCount = (orders.data ?? []).filter((o) => o.card.verify).length;
  const groups = useMemo(() => {
    const shown = (orders.data ?? []).filter((o) => filter === "all" || o.card.verify);
    const by = new Map<string, Order[]>();
    for (const o of shown) {
      const k = dayKey(o.at);
      by.set(k, [...(by.get(k) ?? []), o]);
    }
    return [...by.entries()];
  }, [orders.data, filter]);

  return (
    <>
      <PageHeader
        title="הזמנות"
        subtitle="כל משימה שהסוכן השלים, עם כל הפרטים שנאספו"
        action={
          <Segmented
            label="סינון"
            value={filter}
            options={[
              { value: "all", label: "הכל", count: orders.data?.length },
              { value: "verify", label: "לבדיקה", count: orders.data ? verifyCount : undefined },
            ]}
            onChange={(v) => setParams(v === "verify" ? { verify: "1" } : {})}
          />
        }
      />
      {orders.error && <Problem>{orders.error}</Problem>}
      {!orders.data && !orders.error ? (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {Array.from({ length: 6 }, (_, i) => (
            <Skeleton key={i} className="h-64 rounded-2xl" />
          ))}
        </div>
      ) : groups.length === 0 ? (
        <div className={SURFACE}>
          <Empty icon={<CarTaxiFront className="size-5" aria-hidden />}>{filter === "verify" ? "אין הזמנות לבדיקה. הכול נקלט." : "אין הזמנות עדיין"}</Empty>
        </div>
      ) : (
        <div className="space-y-8">
          {groups.map(([key, list]) => (
            <section key={key}>
              <h2 className="mb-3 flex items-center gap-3 text-sm font-semibold text-slate-600 dark:text-slate-300">
                {dayTitle(key)}
                <span className="num rounded-full bg-slate-200/70 px-2 py-0.5 text-xs font-medium text-slate-600 dark:bg-white/10 dark:text-slate-300">{list.length}</span>
                <span className="h-px flex-1 bg-slate-200 dark:bg-white/[0.08]" aria-hidden />
              </h2>
              <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
                {list.map((o, i) => (
                  <OrderCard key={`${o.call_id}-${i}`} order={o} />
                ))}
              </div>
            </section>
          ))}
        </div>
      )}
    </>
  );
}

const KNOWN = ["pickup", "destination", "pickup_time", "passengers", "customer_name", "notes"];

function OrderCard({ order }: { order: Order }) {
  const { card } = order;
  const result = card.result ?? {};
  const rideId = typeof result.ride_id === "string" ? result.ride_id : null;
  const eta = typeof result.eta_minutes === "number" ? result.eta_minutes : null;
  const pickup = card.details.find((d) => d.field === "pickup");
  const destination = card.details.find((d) => d.field === "destination");
  const notes = detail(order, "notes");
  const extra = card.details.filter((d) => !KNOWN.includes(d.field) && d.value.trim());
  const call = phone(card.phone ?? order.from);
  return (
    <article className={cx(SURFACE, "flex animate-rise flex-col overflow-hidden", card.verify && "ring-2 ring-amber-400/70 dark:ring-amber-400/50")}>
      {card.verify && (
        <div className="flex items-center gap-2 bg-amber-50 px-5 py-2 text-xs font-medium text-amber-900 dark:bg-amber-400/10 dark:text-amber-200">
          <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
          לא ידוע אם ההזמנה נקלטה. צריך לוודא מול המוקד.
        </div>
      )}
      <header className="flex items-center justify-between gap-3 px-5 pt-4">
        <div className="flex items-center gap-2.5">
          <span className="flex size-9 items-center justify-center rounded-xl bg-brand-50 text-brand-600 dark:bg-brand-400/15 dark:text-brand-200">
            <CarTaxiFront className="size-[18px]" aria-hidden />
          </span>
          <div>
            <div className="num text-sm font-semibold">{hhmm(order.at)}</div>
            <div className="text-xs text-slate-500 dark:text-slate-400">{card.task}</div>
          </div>
        </div>
        {card.verify ? <Badge tone="warn" dot>לבדיקה</Badge> : <Badge tone="good" dot>נקלטה</Badge>}
      </header>

      <div className="px-5 pt-4">
        <Route from={pickup ? <Place d={pickup} /> : "—"} to={destination ? <Place d={destination} /> : "—"} />
      </div>

      <div className="mt-4 flex flex-wrap gap-2 px-5">
        {detail(order, "passengers") && <Chip icon={<Users className="size-3.5" aria-hidden />}>{detail(order, "passengers")} נוסעים</Chip>}
        {detail(order, "pickup_time") && <Chip icon={<Clock className="size-3.5" aria-hidden />}>{detail(order, "pickup_time")}</Chip>}
        {detail(order, "customer_name") && <Chip icon={<User className="size-3.5" aria-hidden />}>{detail(order, "customer_name")}</Chip>}
        {extra.map((d) => (
          <Chip key={d.field}>
            {d.label}: {d.value}
          </Chip>
        ))}
      </div>

      {notes && (
        <div className="mx-5 mt-3 flex items-start gap-2 rounded-xl bg-amber-50/70 px-3 py-2 text-sm text-amber-950 dark:bg-amber-400/10 dark:text-amber-100">
          <StickyNote className="mt-0.5 size-4 shrink-0 text-amber-600 dark:text-amber-300" aria-hidden />
          <span>
            <span className="text-xs font-medium text-amber-700 dark:text-amber-300">לנהג · </span>
            {notes}
          </span>
        </div>
      )}

      <footer className="mt-5 flex flex-wrap items-center gap-x-4 gap-y-2 border-t border-slate-100 px-5 py-3 text-xs text-slate-500 dark:border-white/[0.06] dark:text-slate-400">
        {call !== "—" && (
          <a href={`tel:${card.phone ?? order.from ?? ""}`} className="ltr num inline-flex items-center gap-1.5 font-medium text-slate-700 hover:text-brand-700 dark:text-slate-200 dark:hover:text-brand-300">
            <Phone className="size-3.5" aria-hidden />
            {call}
          </a>
        )}
        {rideId && <span className="ltr num">#{rideId}</span>}
        {eta != null && <span>הגעה בעוד {eta} דק׳</span>}
        <Link to={`/calls/${order.call_id}`} className="ms-auto inline-flex items-center gap-1 font-medium text-brand-700 hover:underline dark:text-brand-300">
          לשיחה
          <ArrowLeft className="size-3.5" aria-hidden />
        </Link>
      </footer>
    </article>
  );
}

function Place({ d }: { d: { value: string; address: string | null } }) {
  return (
    <>
      {d.value}
      {d.address && d.address !== d.value && <span className="mt-0.5 block text-xs font-normal text-slate-500 dark:text-slate-400">{d.address}</span>}
    </>
  );
}

function Chip({ icon, children }: { icon?: React.ReactNode; children: React.ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1.5 rounded-lg bg-slate-100 px-2.5 py-1 text-xs font-medium text-slate-700 dark:bg-white/[0.07] dark:text-slate-200">
      {icon}
      {children}
    </span>
  );
}
