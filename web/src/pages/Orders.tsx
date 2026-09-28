import { useMemo } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { Phone } from "lucide-react";
import { type Order, useApi } from "../api";
import { phone } from "../format";
import { Badge, Card, Empty, Loading, PageHeader, Problem, Segmented, When } from "../ui";

type Filter = "all" | "verify";

export function Orders() {
  const [params, setParams] = useSearchParams();
  const filter: Filter = params.get("verify") ? "verify" : "all";
  const orders = useApi<Order[]>("/api/orders", 30_000);

  const shown = useMemo(() => (orders.data ?? []).filter((o) => filter === "all" || o.card.verify), [orders.data, filter]);

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
              { value: "all", label: "הכל" },
              { value: "verify", label: "לבדיקה" },
            ]}
            onChange={(v) => setParams(v === "verify" ? { verify: "1" } : {})}
          />
        }
      />
      {orders.error && <Problem>{orders.error}</Problem>}
      {!orders.data && !orders.error ? (
        <Loading />
      ) : shown.length === 0 ? (
        <Card>
          <Empty>{filter === "verify" ? "אין הזמנות לבדיקה" : "אין הזמנות עדיין"}</Empty>
        </Card>
      ) : (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {shown.map((o, i) => (
            <OrderCard key={`${o.call_id}-${i}`} order={o} />
          ))}
        </div>
      )}
    </>
  );
}

function OrderCard({ order }: { order: Order }) {
  const { card } = order;
  const result = card.result ?? {};
  const rideId = typeof result.ride_id === "string" ? result.ride_id : null;
  const eta = typeof result.eta_minutes === "number" ? result.eta_minutes : null;
  return (
    <article className="flex flex-col rounded-xl border border-slate-200 bg-white shadow-sm dark:border-slate-800 dark:bg-slate-900">
      <header className="flex items-start justify-between gap-3 border-b border-slate-100 px-5 py-3.5 dark:border-slate-800">
        <div>
          <h2 className="font-semibold">{card.task}</h2>
          <When iso={order.at} className="text-xs text-slate-500 dark:text-slate-400" />
        </div>
        {card.verify ? <Badge tone="warn">לבדיקה</Badge> : <Badge tone="good">נקלטה</Badge>}
      </header>
      <dl className="flex-1 divide-y divide-slate-100 px-5 text-sm dark:divide-slate-800">
        {card.details.map((d) => (
          <div key={d.field} className="grid grid-cols-[7rem_1fr] gap-3 py-2">
            <dt className="text-slate-500 dark:text-slate-400">{d.label}</dt>
            <dd className="font-medium">
              {d.value}
              {d.address && d.address !== d.value && <div className="text-xs font-normal text-slate-500">{d.address}</div>}
            </dd>
          </div>
        ))}
      </dl>
      <footer className="flex flex-wrap items-center gap-3 border-t border-slate-100 px-5 py-3 text-xs text-slate-500 dark:border-slate-800 dark:text-slate-400">
        <span className="inline-flex items-center gap-1">
          <Phone className="size-3.5" aria-hidden />
          <span className="ltr tabular-nums">{phone(card.phone ?? order.from)}</span>
        </span>
        {rideId && <span className="ltr">#{rideId}</span>}
        {eta != null && <span>הגעה בעוד {eta} דק׳</span>}
        <Link to={`/calls/${order.call_id}`} className="ms-auto font-medium text-brand-700 hover:underline dark:text-brand-300">
          לשיחה
        </Link>
      </footer>
    </article>
  );
}
