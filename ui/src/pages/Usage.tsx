import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, type Billing, type LabelUsage, type LabelUsageRow } from "../api";
import { CurlDialog, type CurlRequest } from "../components/CurlButton";
import { Banner, Button, MenuButton, MultiSelect, TimeRange, useTimeRange } from "../components/controls";
import { KeyFilter, setKeyFilter } from "../components/KeyFilter";
import { LabelFilter, useLabelSuggestions } from "../components/LabelInput";
import { ErrorBanner, Modal, SortHeader, type Sort } from "../components/ui";
import { compact, usd } from "../format";
import { labelQuery, type Labels } from "../labels";
import { useSessionState } from "../sessionState";
import { download, xlsx, type Cell } from "../xlsx";

const RANGES = ["24h", "7d", "month", "last_month", "custom"] as const;

/** The label keys the grid groups by at most (`/metrics/labels`'s `by`). */
const MAX_KEYS = 3;

/** The totals' columns; the label columns sort by their index, as `label:0`. */
type Metric = "requests" | "input" | "cache_read" | "cache_write" | "output" | "cost";
type Column = Metric | `label:${number}`;

const METRICS: { id: Metric; label: string; sheet: string; value: (r: LabelUsageRow) => number; format: (n: number) => string }[] = [
  { id: "requests", label: "Requests", sheet: "Requests", value: (r) => r.requests, format: compact },
  { id: "input", label: "Input", sheet: "Input tokens", value: (r) => r.input_tokens, format: compact },
  { id: "cache_read", label: "Cache read", sheet: "Cache read tokens", value: (r) => r.cache_read_tokens, format: compact },
  { id: "cache_write", label: "Cache write", sheet: "Cache write tokens", value: (r) => r.cache_write_tokens, format: compact },
  { id: "output", label: "Output", sheet: "Output tokens", value: (r) => r.output_tokens, format: compact },
  { id: "cost", label: "Cost", sheet: "Cost (USD)", value: (r) => r.cost_usd, format: usd },
];

/** The rows' sum, as a row. */
function total(rows: LabelUsageRow[]): LabelUsageRow {
  const sum = (f: (r: LabelUsageRow) => number) => rows.reduce((n, r) => n + f(r), 0);
  return {
    requests: sum((r) => r.requests),
    input_tokens: sum((r) => r.input_tokens),
    cache_read_tokens: sum((r) => r.cache_read_tokens),
    cache_write_tokens: sum((r) => r.cache_write_tokens),
    output_tokens: sum((r) => r.output_tokens),
    cost_usd: sum((r) => r.cost_usd),
  };
}

/** The last day of a range whose `to` is exclusive, as YYYY-MM-DD. */
const lastDay = (to: string) => new Date(new Date(to).getTime() - 1).toISOString().slice(0, 10);

/** A sheet's closing lines: the total, then the period. */
function sheetEnd(rows: LabelUsageRow[], lead: number, from: string, to: string): Cell[][] {
  const t = total(rows);
  return [
    ["Total", ...Array<Cell>(lead - 1).fill(null), ...METRICS.map((m) => m.value(t))],
    [],
    ["Period (UTC)", `${from.slice(0, 10)} to ${lastDay(to)}`],
  ];
}

/** The grid as a sheet: a column per label key (empty without that label), then the totals. */
function usageSheet(by: string[], u: LabelUsage, rows: LabelUsageRow[]): Blob {
  const sheet: Cell[][] = [
    [...by, ...METRICS.map((m) => m.sheet)],
    ...rows.map((r) => [...(r.values ?? []), ...METRICS.map((m) => m.value(r))]),
    ...sheetEnd(rows, by.length, u.from, u.to),
  ];
  return xlsx([{ name: "Usage by label", rows: sheet, widths: [...by.map(() => 24), 11, 14, 18, 19, 14, 12] }]);
}

/** The billing export: one sheet with a row per project, key and model, a total, and the period. */
function billingSheet(b: Billing): Blob {
  const sum = (f: (r: Billing["rows"][number]) => number) => b.rows.reduce((n, r) => n + f(r), 0);
  const rows: Cell[][] = [
    ["Project", "Key", "Model", "Requests", "Input tokens", "Cache read tokens", "Cache write tokens", "Output tokens", "Cost (USD)"],
    ...b.rows.map((r) => [
      r.project,
      r.key,
      r.model,
      r.requests,
      r.input_tokens,
      r.cache_read_tokens,
      r.cache_write_tokens,
      r.output_tokens,
      r.cost_usd,
    ]),
    [
      "Total",
      null,
      null,
      sum((r) => r.requests),
      sum((r) => r.input_tokens),
      sum((r) => r.cache_read_tokens),
      sum((r) => r.cache_write_tokens),
      sum((r) => r.output_tokens),
      sum((r) => r.cost_usd),
    ],
    [],
    ["Period (UTC)", `${b.from.slice(0, 10)} to ${lastDay(b.to)}`],
  ];
  return xlsx([{ name: "Billing", rows, widths: [24, 24, 32, 11, 14, 18, 19, 14, 12] }]);
}

/** A label value, or what stands for the requests without that label. */
function LabelValue({ value }: { value: string | null | undefined }) {
  return value == null ? <span className="muted">No label</span> : <>{value}</>;
}

/** The table's totals cells of a row. */
function MetricCells({ row }: { row: LabelUsageRow }) {
  return (
    <>
      {METRICS.map((m) => (
        <td key={m.id} className="num">
          {m.format(m.value(row))}
        </td>
      ))}
    </>
  );
}

/** Token usage and cost per value of up to three request label keys, with a drilldown per model. */
export function Usage() {
  const time = useTimeRange(RANGES, "month", "usage");
  const { query: rangeQuery } = time;
  const [keyId, setKeyId] = useSessionState("usage.key", "");
  const [labels, setLabels] = useSessionState<Labels>("usage.labels", {});
  const [by, setBy] = useSessionState<string[]>("usage.by", []);
  const [labelKeys] = useLabelSuggestions();
  const labelFilter = labelQuery(labels);
  const byParam = by.join(",");

  /** The grid's data, with the label keys it was loaded for. */
  const [loaded, setLoaded] = useState<{ by: string[]; usage: LabelUsage } | null>(null);
  const data = loaded?.usage ?? null;
  const keys = loaded?.by ?? by;
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // Costliest first, as the server sorts.
  const [sort, setSort] = useState<Sort<Column>>({ by: "cost", desc: true });
  const [open, setOpen] = useState<(string | null)[] | null>(null);
  const [curl, setCurl] = useState<CurlRequest | null>(null);
  const [exporting, setExporting] = useState(false);

  /** The time range, key and label filters, as `/metrics/labels` and `/metrics/billing` take them. */
  const filterQuery = useCallback(() => {
    if (rangeQuery === null) return null;
    const q = new URLSearchParams(rangeQuery);
    setKeyFilter(q, keyId);
    if (labelFilter) q.set("labels", labelFilter);
    return q;
  }, [rangeQuery, keyId, labelFilter]);

  /** The grid's request; null without label keys to group by. */
  const usageQuery = useCallback(() => {
    const q = filterQuery();
    if (q === null || !byParam) return null;
    q.set("by", byParam);
    return q;
  }, [filterQuery, byParam]);

  /** The latest load; an earlier one that answers later is dropped. */
  const latest = useRef(0);
  useEffect(() => {
    const id = ++latest.current;
    const q = usageQuery();
    if (q === null) {
      setLoaded(null);
      setLoading(false);
      return;
    }
    const picked = byParam.split(",");
    setLoading(true);
    api<LabelUsage>(`/metrics/labels?${q}`)
      .then((usage) => {
        if (id !== latest.current) return;
        setLoaded({ by: picked, usage });
        setError(null);
      })
      .catch((e) => id === latest.current && setError((e as Error).message))
      .finally(() => id === latest.current && setLoading(false));
  }, [usageQuery, byParam]);

  const rows = useMemo(() => {
    const all = [...(data?.rows ?? [])];
    const label = sort.by.startsWith("label:") ? Number(sort.by.slice(6)) : null;
    const metric = METRICS.find((m) => m.id === sort.by);
    const dir = sort.desc ? -1 : 1;
    all.sort((a, b) => {
      if (label !== null) {
        // Requests without the label go last either way.
        const [x, y] = [a.values?.[label] ?? null, b.values?.[label] ?? null];
        if (x === null || y === null) return x === y ? 0 : x === null ? 1 : -1;
        return dir * x.localeCompare(y, undefined, { numeric: true, sensitivity: "base" });
      }
      return metric ? dir * (metric.value(a) - metric.value(b)) : 0;
    });
    return all;
  }, [data, sort]);

  // A label column that is gone sorts by cost again.
  if (sort.by.startsWith("label:") && Number(sort.by.slice(6)) >= keys.length) setSort({ by: "cost", desc: true });

  const run = async (f: () => Promise<void>) => {
    setExporting(true);
    try {
      await f();
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setExporting(false);
    }
  };
  const exportUsage = () =>
    data &&
    run(async () => {
      download(usageSheet(keys, data, rows), `usage-by-${keys.join("-")}-${data.from.slice(0, 10)}-to-${lastDay(data.to)}.xlsx`);
    });
  const exportBilling = () => {
    const q = filterQuery();
    if (q === null) return;
    run(async () => {
      const b = await api<Billing>(`/metrics/billing?${q}`);
      download(billingSheet(b), `billing-${b.from.slice(0, 10)}-to-${lastDay(b.to)}.xlsx`);
    });
  };

  const keyOptions = useMemo(() => {
    const known = labelKeys.map((k) => ({
      value: k.key,
      label: k.key,
      note: k.values.length === 1 ? "1 value" : `${k.values.length}${k.values.length >= 50 ? "+" : ""} values`,
    }));
    // Picked keys stay choosable after they drop out of the latest requests' sample.
    const options = [...known, ...by.filter((k) => !labelKeys.some((l) => l.key === k)).map((k) => ({ value: k, label: k }))];
    const full = by.length >= MAX_KEYS;
    return options.map((o) => ({ ...o, disabled: full && !by.includes(o.value) }));
  }, [labelKeys, by]);

  const head = (id: Column, label: string, num = true) => (
    <SortHeader key={id} id={id} sort={sort} onSort={setSort} num={num} ascFirst={!num}>
      {label}
    </SortHeader>
  );

  return (
    <div className={loading && data ? "page refreshing" : "page"}>
      <div className="page-head">
        <div className="filters">
          <TimeRange {...time.props} />
          <KeyFilter value={keyId} onChange={setKeyId} />
          <LabelFilter value={labels} onChange={setLabels} />
          <MultiSelect
            caption="Group by"
            hint={`Up to ${MAX_KEYS} label keys whose values the rows total: a row for each combination of their values. Requests without a label count under No label. Click a row for its usage per model.`}
            values={by}
            onChange={(v) => setBy(v.slice(0, MAX_KEYS))}
            options={keyOptions}
            none="No label key"
            required
          />
        </div>
        <div className="page-actions">
          <MenuButton
            label="Export"
            className="more-button primary"
            items={[
              {
                label: "Usage by label (.xlsx)",
                title: "This table: token usage and cost per label value, with these filters.",
                onClick: exportUsage,
                disabled: !data || data.rows.length === 0 || exporting,
              },
              {
                label: "Billing (.xlsx)",
                title: "Token usage and cost per project, key and model, with these filters.",
                onClick: exportBilling,
                disabled: rangeQuery === null || exporting,
              },
              {
                label: "Show curl",
                title: "This table's request, or without label keys the billing totals, as a curl command for the admin API.",
                onClick: () => {
                  const q = usageQuery();
                  if (q) setCurl({ path: `/admin/api/metrics/labels?${q}` });
                  else {
                    const f = filterQuery();
                    if (f) setCurl({ path: `/admin/api/metrics/billing?${f}` });
                  }
                },
                disabled: rangeQuery === null,
              },
            ]}
          >
            Export
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" aria-hidden="true">
              <path d="m6 9 6 6 6-6" />
            </svg>
          </MenuButton>
        </div>
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {curl && (
        <CurlDialog
          request={curl}
          note={
            <>
              Set <code>GATEWAY_API_KEY</code> to the master key, an admin key, or a project key.
            </>
          }
          onClose={() => setCurl(null)}
        />
      )}
      {data?.truncated && (
        <Banner tone="warning">
          The {compact(data.rows.length)} costliest rows are shown. Group by fewer keys or filter to see the rest.
        </Banner>
      )}

      {by.length === 0 ? (
        <div className="empty-card">Pick label keys under Group by to total usage per label value.</div>
      ) : (
        data && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  {keys.map((k, i) => head(`label:${i}`, k, false))}
                  {METRICS.map((m) => head(m.id, m.label))}
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => {
                  const values = r.values ?? [];
                  const show = () => setOpen(values);
                  return (
                    <tr
                      key={JSON.stringify(values)}
                      className="clickable"
                      tabIndex={0}
                      title="Usage per model"
                      onClick={show}
                      onKeyDown={(e) => {
                        if (e.target === e.currentTarget && (e.key === "Enter" || e.key === " ")) {
                          e.preventDefault();
                          show();
                        }
                      }}
                    >
                      {keys.map((k, i) => (
                        <td key={k}>
                          <LabelValue value={values[i]} />
                        </td>
                      ))}
                      <MetricCells row={r} />
                    </tr>
                  );
                })}
                {rows.length === 0 && (
                  <tr>
                    <td colSpan={keys.length + METRICS.length} className="empty">
                      No requests in this time range.
                    </td>
                  </tr>
                )}
              </tbody>
              {rows.length > 1 && (
                <tfoot>
                  <tr>
                    <td colSpan={keys.length}>Total</td>
                    <MetricCells row={total(rows)} />
                  </tr>
                </tfoot>
              )}
            </table>
          </div>
        )
      )}

      {open && <ModelDrilldown by={keys} values={open} query={filterQuery()} onClose={() => setOpen(null)} />}
    </div>
  );
}

/** One label value's usage per model, in a dialog. */
function ModelDrilldown({
  by,
  values,
  query,
  onClose,
}: {
  by: string[];
  values: (string | null)[];
  query: URLSearchParams | null;
  onClose: () => void;
}) {
  const [data, setData] = useState<LabelUsage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const path = useMemo(() => {
    if (query === null) return null;
    const q = new URLSearchParams(query);
    q.set("by", by.join(","));
    q.set("values", JSON.stringify(values));
    return `/metrics/labels?${q}`;
  }, [query, by, values]);

  useEffect(() => {
    if (path === null) return;
    let live = true;
    api<LabelUsage>(path)
      .then((d) => live && setData(d))
      .catch((e) => live && setError((e as Error).message));
    return () => {
      live = false;
    };
  }, [path]);

  const rows = data?.rows ?? [];
  const title = by.map((k, i) => `${k}=${values[i] ?? "no label"}`).join(", ");
  const exportModels = () => {
    if (!data) return;
    const sheet: Cell[][] = [
      ["Model", ...METRICS.map((m) => m.sheet)],
      ...rows.map((r) => [r.model ?? "", ...METRICS.map((m) => m.value(r))]),
      ...sheetEnd(rows, 1, data.from, data.to),
      ["Labels", title],
    ];
    const name = by
      .map((k, i) => `${k}-${values[i] ?? "none"}`)
      .join("-")
      .replace(/[^A-Za-z0-9_.-]+/g, "_");
    download(xlsx([{ name: "Usage by model", rows: sheet, widths: [32, 11, 14, 18, 19, 14, 12] }]), `usage-${name}-by-model.xlsx`);
  };

  return (
    <Modal
      title={`Usage per model: ${title}`}
      size="wide"
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={exportModels} disabled={rows.length === 0}>
            Export (.xlsx)
          </Button>
          <Button variant="primary" onClick={onClose}>
            Close
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {!data && !error && <p className="muted">Loading…</p>}
      {data && (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Model</th>
                {METRICS.map((m) => (
                  <th key={m.id} className="num">
                    {m.label}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.model ?? ""}>
                  <td className="model">{r.model}</td>
                  <MetricCells row={r} />
                </tr>
              ))}
              {rows.length === 0 && (
                <tr>
                  <td colSpan={1 + METRICS.length} className="empty">
                    No requests in this time range.
                  </td>
                </tr>
              )}
            </tbody>
            {rows.length > 1 && (
              <tfoot>
                <tr>
                  <td>Total</td>
                  <MetricCells row={total(rows)} />
                </tr>
              </tfoot>
            )}
          </table>
        </div>
      )}
    </Modal>
  );
}
