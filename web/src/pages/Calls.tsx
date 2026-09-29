import { useEffect, useMemo, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { ChevronLeft, PhoneOff, Search, X } from "lucide-react";
import { api, type CallRow } from "../api";
import { duration, phone, tokens } from "../format";
import { Ago, Button, Empty, PageHeader, Problem, Segmented, Skeleton, SURFACE, cx } from "../ui";
import { CallAvatar, OutcomeBadge, VerdictBadge } from "../parts";

const PAGE = 100;

type Filter = "all" | "orders" | "handed" | "nothing" | "bad";

const FILTERS: { value: Filter; label: string }[] = [
  { value: "all", label: "הכל" },
  { value: "orders", label: "עם הזמנה" },
  { value: "handed", label: "הועבר למוקדן" },
  { value: "nothing", label: "בלי תוצאה" },
  { value: "bad", label: "לא תקין" },
];

function matches(c: CallRow, filter: Filter): boolean {
  switch (filter) {
    case "orders":
      return c.orders > 0;
    case "handed":
      return c.outcome === "HandedOff";
    case "nothing":
      return c.orders === 0 && c.outcome !== "HandedOff" && c.outcome !== null;
    case "bad":
      return c.verdict === "bad";
    default:
      return true;
  }
}

export function Calls() {
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const asked = params.get("filter");
  const filter: Filter = FILTERS.some((f) => f.value === asked) ? (asked as Filter) : "all";
  const [rows, setRows] = useState<CallRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [more, setMore] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [query, setQuery] = useState("");

  const setFilter = (f: Filter) => setParams(f === "all" ? {} : { filter: f }, { replace: true });

  const load = async (offset: number) => {
    const page = await api<CallRow[]>(`/api/calls?limit=${PAGE}&offset=${offset}`);
    setMore(page.length === PAGE);
    return page;
  };

  useEffect(() => {
    let alive = true;
    const refresh = () =>
      load(0)
        .then((page) => {
          if (!alive) return;
          // Keep what was loaded beyond the first page.
          setRows((old) => (old && old.length > PAGE ? [...page, ...old.slice(PAGE)] : page));
          setError(null);
        })
        .catch((e: Error) => alive && setError(e.message === "503" ? "אין חיבור למסד הנתונים" : "השיחות לא נטענו"));
    void refresh();
    const timer = window.setInterval(() => document.visibilityState === "visible" && void refresh(), 30_000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, []);

  const loadMore = async () => {
    if (!rows) return;
    setLoadingMore(true);
    try {
      const page = await load(rows.length);
      setRows([...rows, ...page]);
    } catch {
      setError("השיחות לא נטענו");
    } finally {
      setLoadingMore(false);
    }
  };

  const digits = query.replace(/\D/g, "");
  const bySearch = useMemo(
    () =>
      (rows ?? []).filter((c) => {
        if (!digits) return true;
        const from = (c.from ?? "").replace(/\D/g, "");
        return from.includes(digits) || from.includes(digits.replace(/^0/, "972"));
      }),
    [rows, digits],
  );
  const counts = useMemo(() => Object.fromEntries(FILTERS.map((f) => [f.value, bySearch.filter((c) => matches(c, f.value)).length])) as Record<Filter, number>, [bySearch]);
  const shown = useMemo(() => bySearch.filter((c) => matches(c, filter)), [bySearch, filter]);

  return (
    <>
      <PageHeader title="שיחות" subtitle="כל שיחה, מה נאמר בה ומה הסוכן החליט" />

      <div className="mb-4 flex flex-wrap items-center gap-3">
        <Segmented label="סינון" value={filter} options={FILTERS.map((f) => ({ ...f, count: rows ? counts[f.value] : undefined }))} onChange={setFilter} />
        <div className="relative ms-auto w-full sm:w-72">
          <Search className="pointer-events-none absolute inset-y-0 start-3.5 my-auto size-4 text-slate-400" aria-hidden />
          <input
            type="search"
            inputMode="tel"
            placeholder="חיפוש לפי מספר טלפון"
            aria-label="חיפוש לפי מספר טלפון"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            className="w-full rounded-xl border-0 bg-white py-2.5 ps-10 pe-9 text-sm shadow-sm ring-1 ring-inset ring-slate-300/80 outline-none transition placeholder:text-slate-400 focus:ring-2 focus:ring-brand-500 dark:bg-white/[0.05] dark:ring-white/10 dark:shadow-none dark:focus:ring-brand-400 [&::-webkit-search-cancel-button]:hidden"
          />
          {query && (
            <button type="button" aria-label="ניקוי החיפוש" onClick={() => setQuery("")} className="absolute inset-y-0 end-2.5 my-auto flex size-6 items-center justify-center rounded-md text-slate-400 hover:bg-slate-100 hover:text-slate-600 dark:hover:bg-white/10">
              <X className="size-3.5" aria-hidden />
            </button>
          )}
        </div>
      </div>

      {error && <Problem>{error}</Problem>}

      <div className={cx(SURFACE, "overflow-hidden")}>
        {!rows && !error ? (
          <div className="divide-y divide-slate-100 dark:divide-white/[0.06]">
            {Array.from({ length: 8 }, (_, i) => (
              <div key={i} className="flex items-center gap-4 px-5 py-4">
                <Skeleton className="size-10 rounded-full" />
                <div className="flex-1 space-y-2">
                  <Skeleton className="h-3.5 w-32" />
                  <Skeleton className="h-3 w-20" />
                </div>
                <Skeleton className="h-6 w-24 rounded-full" />
              </div>
            ))}
          </div>
        ) : shown.length === 0 ? (
          <Empty
            icon={<PhoneOff className="size-5" aria-hidden />}
            action={
              (filter !== "all" || query) && rows?.length ? (
                <Button
                  onClick={() => {
                    setFilter("all");
                    setQuery("");
                  }}
                >
                  ניקוי הסינון
                </Button>
              ) : undefined
            }
          >
            {rows?.length ? "אין שיחות שמתאימות לסינון" : "אין שיחות עדיין"}
          </Empty>
        ) : (
          <>
            {/* A table where there is room ... */}
            <table className="hidden w-full text-sm md:table">
              <thead>
                <tr className="border-b border-slate-100 text-xs text-slate-500 dark:border-white/[0.06] dark:text-slate-400">
                  <th className="px-5 py-3 text-start font-medium">שיחה</th>
                  <th className="px-3 py-3 text-start font-medium">תוצאה</th>
                  <th className="px-3 py-3 text-start font-medium">משך</th>
                  <th className="px-3 py-3 text-start font-medium">הזמנות</th>
                  <th className="hidden px-3 py-3 text-start font-medium xl:table-cell">טוקנים</th>
                  <th className="px-3 py-3 text-start font-medium">בדיקה</th>
                  <th className="w-10" />
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-100 dark:divide-white/[0.06]">
                {shown.map((c) => (
                  <tr
                    key={c.id}
                    tabIndex={0}
                    onClick={() => navigate(`/calls/${c.id}`)}
                    onKeyDown={(e) => e.key === "Enter" && navigate(`/calls/${c.id}`)}
                    className="group cursor-pointer transition-colors hover:bg-slate-50 dark:hover:bg-white/[0.04]"
                  >
                    <td className="px-5 py-3">
                      <div className="flex items-center gap-3.5">
                        <CallAvatar outcome={c.outcome} orders={c.orders} />
                        <div>
                          <div className="ltr num text-start font-semibold">{phone(c.from)}</div>
                          <Ago iso={c.started_at} className="text-xs text-slate-500 dark:text-slate-400" />
                        </div>
                      </div>
                    </td>
                    <td className="px-3 py-3">
                      <OutcomeBadge outcome={c.outcome} orders={c.orders} />
                    </td>
                    <td className="num px-3 py-3 text-slate-700 dark:text-slate-300">{duration(c.duration_seconds)}</td>
                    <td className="num px-3 py-3 text-slate-700 dark:text-slate-300">{c.orders || <span className="text-slate-300 dark:text-slate-600">—</span>}</td>
                    <td className="num hidden px-3 py-3 text-slate-500 xl:table-cell dark:text-slate-400">{c.usage ? tokens(c.usage.input + c.usage.output) : "—"}</td>
                    <td className="px-3 py-3">
                      <VerdictBadge verdict={c.verdict} />
                    </td>
                    <td className="pe-4">
                      <ChevronLeft className="size-4 text-slate-300 transition-transform group-hover:-translate-x-0.5 group-hover:text-slate-500 dark:text-slate-600" aria-hidden />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>

            {/* ... and a list on a phone. */}
            <ul className="divide-y divide-slate-100 md:hidden dark:divide-white/[0.06]">
              {shown.map((c) => (
                <li key={c.id}>
                  <button type="button" onClick={() => navigate(`/calls/${c.id}`)} className="flex w-full items-center gap-3.5 px-4 py-3.5 text-start transition-colors active:bg-slate-50 dark:active:bg-white/[0.04]">
                    <CallAvatar outcome={c.outcome} orders={c.orders} />
                    <div className="min-w-0 flex-1">
                      <div className="ltr num text-start text-sm font-semibold">{phone(c.from)}</div>
                      <div className="mt-0.5 flex flex-wrap items-center gap-x-2 text-xs text-slate-500 dark:text-slate-400">
                        <Ago iso={c.started_at} />
                        <span aria-hidden>·</span>
                        <span className="num">{duration(c.duration_seconds)}</span>
                      </div>
                    </div>
                    <div className="flex flex-col items-end gap-1.5">
                      <OutcomeBadge outcome={c.outcome} orders={c.orders} />
                      <VerdictBadge verdict={c.verdict} />
                    </div>
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}
      </div>

      {rows && shown.length > 0 && (
        <div className="mt-4 flex flex-col items-center gap-3">
          <p className="text-xs text-slate-500 dark:text-slate-400">
            מוצגות {shown.length} מתוך {rows.length} שיחות שנטענו
          </p>
          {more && (
            <Button onClick={loadMore} disabled={loadingMore}>
              {loadingMore ? "טוען…" : "טען עוד שיחות"}
            </Button>
          )}
        </div>
      )}
    </>
  );
}
