import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { Search } from "lucide-react";
import { api, type CallRow } from "../api";
import { duration, OUTCOMES, phone, tokens } from "../format";
import { Badge, Button, Card, Empty, Loading, PageHeader, Problem, Segmented, When } from "../ui";

const PAGE = 100;

type Filter = "all" | "orders" | "handed" | "nothing" | "bad";

const FILTERS: { value: Filter; label: string }[] = [
  { value: "all", label: "הכל" },
  { value: "orders", label: "עם הזמנה" },
  { value: "handed", label: "הועבר למוקדן" },
  { value: "nothing", label: "בלי תוצאה" },
  { value: "bad", label: "סומנו לא תקין" },
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

export function OutcomeBadge({ outcome }: { outcome: string | null }) {
  if (!outcome) return <Badge tone="brand">בשיחה</Badge>;
  const tone = outcome === "HandedOff" ? "warn" : "neutral";
  return <Badge tone={tone}>{OUTCOMES[outcome] ?? outcome}</Badge>;
}

export function VerdictBadge({ verdict }: { verdict: CallRow["verdict"] }) {
  if (!verdict) return null;
  return <Badge tone={verdict === "good" ? "good" : "bad"}>{verdict === "good" ? "תקין" : "לא תקין"}</Badge>;
}

export function Calls() {
  const navigate = useNavigate();
  const [rows, setRows] = useState<CallRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [more, setMore] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [filter, setFilter] = useState<Filter>("all");
  const [query, setQuery] = useState("");

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

  const shown = useMemo(() => {
    const digits = query.replace(/\D/g, "");
    return (rows ?? []).filter((c) => {
      if (!matches(c, filter)) return false;
      if (!digits) return true;
      const from = (c.from ?? "").replace(/\D/g, "");
      return from.includes(digits) || from.includes(digits.replace(/^0/, "972"));
    });
  }, [rows, filter, query]);

  return (
    <>
      <PageHeader title="שיחות" subtitle="כל שיחה, מה נאמר בה ומה הסוכן החליט" />
      <Card>
        <div className="-mt-1 mb-4 flex flex-wrap items-center gap-3">
          <Segmented label="סינון" value={filter} options={FILTERS} onChange={setFilter} />
          <div className="relative ms-auto w-full sm:w-64">
            <Search className="pointer-events-none absolute inset-y-0 start-3 my-auto size-4 text-slate-400" aria-hidden />
            <input
              type="search"
              inputMode="tel"
              placeholder="חיפוש לפי מספר"
              aria-label="חיפוש לפי מספר"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              className="w-full rounded-lg border border-slate-300 bg-white py-2 ps-9 pe-3 text-sm outline-none focus:border-brand-500 focus:ring-2 focus:ring-brand-200 dark:border-slate-700 dark:bg-slate-950 dark:focus:ring-brand-900"
            />
          </div>
        </div>

        {error && <Problem>{error}</Problem>}
        {!rows && !error ? (
          <Loading />
        ) : shown.length === 0 ? (
          <Empty>{rows?.length ? "אין שיחות שמתאימות לסינון" : "אין שיחות עדיין"}</Empty>
        ) : (
          <div className="-mx-5 overflow-x-auto">
            <table className="w-full min-w-[720px] text-sm">
              <thead>
                <tr className="border-y border-slate-100 bg-slate-50 text-xs text-slate-500 dark:border-slate-800 dark:bg-slate-900/60 dark:text-slate-400">
                  <th className="px-5 py-2.5 text-start font-medium">זמן</th>
                  <th className="px-3 py-2.5 text-start font-medium">מספר</th>
                  <th className="px-3 py-2.5 text-start font-medium">תוצאה</th>
                  <th className="px-3 py-2.5 text-start font-medium">משך</th>
                  <th className="px-3 py-2.5 text-start font-medium">הזמנות</th>
                  <th className="px-3 py-2.5 text-start font-medium">טוקנים</th>
                  <th className="px-5 py-2.5 text-start font-medium">בדיקה</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-100 dark:divide-slate-800">
                {shown.map((c) => (
                  <tr
                    key={c.id}
                    tabIndex={0}
                    onClick={() => navigate(`/calls/${c.id}`)}
                    onKeyDown={(e) => e.key === "Enter" && navigate(`/calls/${c.id}`)}
                    className="cursor-pointer transition-colors hover:bg-slate-50 dark:hover:bg-slate-800/50"
                  >
                    <td className="whitespace-nowrap px-5 py-3 text-slate-600 dark:text-slate-300">
                      <When iso={c.started_at} />
                    </td>
                    <td className="px-3 py-3">
                      <span className="ltr font-medium tabular-nums">{phone(c.from)}</span>
                    </td>
                    <td className="px-3 py-3">
                      <OutcomeBadge outcome={c.outcome} />
                    </td>
                    <td className="px-3 py-3 tabular-nums">{duration(c.duration_seconds)}</td>
                    <td className="px-3 py-3 tabular-nums">{c.orders || "—"}</td>
                    <td className="px-3 py-3 tabular-nums text-slate-500">{c.usage ? tokens(c.usage.input + c.usage.output) : "—"}</td>
                    <td className="px-5 py-3">
                      <VerdictBadge verdict={c.verdict} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {more && rows && (
          <div className="mt-4 flex justify-center">
            <Button onClick={loadMore} disabled={loadingMore}>
              {loadingMore ? "טוען…" : "טען עוד שיחות"}
            </Button>
          </div>
        )}
      </Card>
    </>
  );
}
