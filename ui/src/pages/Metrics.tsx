import { useCallback, useEffect, useState } from "react";
import { api, type Billing, type Metrics as MetricsData } from "../api";
import { CurlDialog, type CurlRequest } from "../components/CurlButton";
import { TimeChart } from "../components/TimeChart";
import { MenuButton, MultiSelect, Segmented, TimeRange, useTimeRange } from "../components/controls";
import { KeyFilter, setKeyFilter } from "../components/KeyFilter";
import { LabelFilter } from "../components/LabelInput";
import { ErrorBanner, SortHeader, StatTile, type Sort } from "../components/ui";
import { compact, ms, pct, usd } from "../format";
import { labelQuery, type Labels } from "../labels";
import { useSessionState } from "../sessionState";
import { download, xlsx, type Cell } from "../xlsx";

const RANGES = ["1h", "24h", "7d", "month", "last_month", "custom"] as const;

const PER: Record<number, string> = { 60: "per minute", 3600: "per hour", 86400: "per day" };

/** The table's columns, by the names `/metrics` sorts them by. */
type Column = "name" | "requests" | "errors" | "input" | "output" | "latency" | "p95" | "ttft" | "cost";

/** What the table's rows total, by `/metrics`'s `group`. */
type Group = "model" | "project" | "key";

const GROUPS: { value: Group; label: string }[] = [
  { value: "model", label: "Model" },
  { value: "project", label: "Project" },
  { value: "key", label: "Key" },
];

/** The `KeyFilter` value of a project's row (see `setKeyFilter`). */
const PROJECT = "project:";

const GROUP_STORE = "metrics.group";

function storedGroup(): Group {
  try {
    const g = localStorage.getItem(GROUP_STORE);
    return GROUPS.some((o) => o.value === g) ? (g as Group) : "model";
  } catch {
    return "model";
  }
}

/** The billing export: one sheet with a row per project, key and model, a total, and the period. */
function billingSheet(b: Billing): Blob {
  const day = (iso: string) => iso.slice(0, 10);
  // `to` is exclusive: the last day is the one before it.
  const last = new Date(new Date(b.to).getTime() - 1).toISOString();
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
    ["Period (UTC)", `${day(b.from)} to ${day(last)}`],
  ];
  return xlsx([{ name: "Billing", rows, widths: [24, 24, 32, 11, 14, 18, 19, 14, 12] }]);
}

/** A row's name: a model's, a project's or key's, or what stands for one without a name. */
function rowName(group: Group, row: MetricsData["rows"][number]): string {
  if (group === "model" || row.name) return row.name ?? row.id ?? "";
  if (row.id) return group === "project" ? "Deleted project" : "Deleted key";
  return group === "project" ? "No project" : "No key";
}

export function Metrics() {
  const time = useTimeRange(RANGES, "month", "metrics");
  const { range, query: rangeQuery, ended } = time;
  const [modelFilter, setModelFilter] = useSessionState<string[]>("metrics.models", []);
  /** The `KeyFilter`'s choice. */
  const [keyId, setKeyId] = useSessionState("metrics.key", "");
  const [labels, setLabels] = useSessionState<Labels>("metrics.labels", {});
  const labelFilter = labelQuery(labels);
  const models = modelFilter.join(",");
  const [data, setData] = useState<MetricsData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // Busiest first, as the server sorts by default.
  const [sort, setSort] = useState<Sort<Column>>({ by: "requests", desc: true });
  const [group, setGroup] = useState<Group>(storedGroup);
  const pickGroup = (g: Group) => {
    setGroup(g);
    try {
      localStorage.setItem(GROUP_STORE, g);
    } catch {
      // Not remembered, then.
    }
  };

  const [curl, setCurl] = useState<CurlRequest | null>(null);

  /** The time range, key, label and model filters, as `/metrics` and `/metrics/billing` take them. */
  const filterQuery = useCallback(() => {
    if (rangeQuery === null) return null;
    const q = new URLSearchParams(rangeQuery);
    setKeyFilter(q, keyId);
    if (labelFilter) q.set("labels", labelFilter);
    if (models) q.set("models", models);
    return q;
  }, [rangeQuery, keyId, labelFilter, models]);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const q = filterQuery();
      if (q === null) return;
      q.set("group", group);
      q.set("sort", sort.by);
      q.set("order", sort.desc ? "desc" : "asc");
      setData(await api<MetricsData>(`/metrics?${q}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, [filterQuery, group, sort]);

  const [exporting, setExporting] = useState(false);
  const exportBilling = async () => {
    const q = filterQuery();
    if (q === null) return;
    setExporting(true);
    try {
      const b = await api<Billing>(`/metrics/billing?${q}`);
      const last = new Date(new Date(b.to).getTime() - 1).toISOString().slice(0, 10);
      download(billingSheet(b), `billing-${b.from.slice(0, 10)}-to-${last}.xlsx`);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setExporting(false);
    }
  };

  useEffect(() => {
    load();
    // A custom range that ended before today does not change.
    if (ended) return;
    const t = setInterval(load, range === "1h" ? 15_000 : 60_000);
    return () => clearInterval(t);
  }, [load, range, ended]);

  const toggleModel = (model: string) => setModelFilter((ms) => (ms.includes(model) ? ms.filter((m) => m !== model) : [...ms, model]));

  // The rows of the group the data was loaded for, sorted by the server.
  const shown: Group = data?.group ?? group;
  const rows = data?.rows ?? [];
  /** The filter a row's click sets: its model, or its project or key in the key filter. */
  const rowFilter = (id: string) => (shown === "project" ? PROJECT + id : id);
  const selected = (id: string | null) => id !== null && (shown === "model" ? modelFilter.includes(id) : keyId === rowFilter(id));
  const pick = (row: MetricsData["rows"][number]) => {
    if (row.id === null) return;
    if (shown === "model") toggleModel(row.id);
    else setKeyId(keyId === rowFilter(row.id) ? "" : rowFilter(row.id));
  };
  const head = (id: Column, label: string, num = true) => (
    <SortHeader id={id} sort={sort} onSort={setSort} num={num} ascFirst={id === "name"}>
      {label}
    </SortHeader>
  );

  const step = data?.step_secs ?? 60;
  const perUnit = PER[step] ?? "per bucket";
  const series = data?.series ?? [];
  const ts = series.map((p) => p.ts);
  const t = data?.totals;

  return (
    <div className={loading && data ? "page refreshing" : "page"}>
      <div className="page-head">
        <div className="filters">
          <TimeRange {...time.props} />
          <KeyFilter value={keyId} onChange={setKeyId} />
          <MultiSelect
            caption="Model"
            values={modelFilter}
            onChange={setModelFilter}
            options={(data?.models ?? []).map((m) => ({ value: m.model, label: m.model, note: m.active ? undefined : "inactive" }))}
          />
          <LabelFilter value={labels} onChange={setLabels} />
        </div>
        <div className="page-actions">
          <MenuButton
            label="Export"
            className="more-button primary"
            items={[
              {
                label: "Billing (.xlsx)",
                title: "Token usage and cost per project, key and model, with these filters.",
                onClick: exportBilling,
                disabled: rangeQuery === null || exporting,
              },
              {
                label: "Show curl",
                title: "The billing totals with these filters, as a curl command for the admin API.",
                onClick: () => {
                  const q = filterQuery();
                  if (q) setCurl({ path: `/admin/api/metrics/billing?${q}` });
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

      {t && (
        <div className="tiles">
          <StatTile label="Requests" value={compact(t.requests)} hint={`${compact(t.errors)} errors`} />
          <StatTile label="Error rate" value={pct(t.requests ? t.errors / t.requests : null)} />
          <StatTile
            label="Input tokens"
            value={compact(t.input_tokens + t.cache_read_tokens + t.cache_write_tokens)}
            hint={`${compact(t.cache_read_tokens)} cache reads`}
          />
          <StatTile label="Output tokens" value={compact(t.output_tokens)} />
          <StatTile label="Cost" value={usd(t.cost_usd)} />
          <StatTile label="Latency" value={ms(t.avg_latency_ms)} hint={`p95 ${ms(t.p95_latency_ms)} · TTFT ${ms(t.avg_ttft_ms)}`} />
        </div>
      )}

      {data && (
        <div className="charts">
          <TimeChart
            kind="bars"
            title="Requests"
            subtitle={perUnit}
            timestamps={ts}
            stepSecs={step}
            format={compact}
            series={[
              { name: "Succeeded", color: "var(--series-1)", values: series.map((p) => p.requests - p.errors) },
              { name: "Failed", color: "var(--status-critical)", values: series.map((p) => p.errors) },
            ]}
          />
          <TimeChart
            kind="line"
            title="Tokens"
            subtitle={perUnit}
            timestamps={ts}
            stepSecs={step}
            format={compact}
            series={[
              { name: "Input", color: "var(--series-1)", values: series.map((p) => p.input_tokens) },
              { name: "Output", color: "var(--series-2)", values: series.map((p) => p.output_tokens) },
            ]}
          />
          <TimeChart
            kind="line"
            title="Average latency"
            subtitle="end-to-end, including streaming"
            timestamps={ts}
            stepSecs={step}
            format={ms}
            series={[{ name: "Latency", color: "var(--series-1)", values: series.map((p) => p.avg_latency_ms) }]}
          />
          <TimeChart
            kind="line"
            title="Cost"
            subtitle={`USD ${perUnit}`}
            timestamps={ts}
            stepSecs={step}
            format={usd}
            series={[{ name: "Cost", color: "var(--series-1)", values: series.map((p) => p.cost_usd) }]}
          />
        </div>
      )}

      {data && (
        <>
          <div className="filters table-bar">
            <Segmented caption="By" label="Totals by" options={GROUPS} value={group} onChange={pickGroup} />
          </div>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  {head("name", GROUPS.find((g) => g.value === shown)!.label, false)}
                  {head("requests", "Requests")}
                  {head("errors", "Errors")}
                  {head("input", "Input")}
                  {head("output", "Output")}
                  {head("latency", "Avg latency")}
                  {head("p95", "p95")}
                  {head("ttft", "TTFT")}
                  {head("cost", "Cost")}
                </tr>
              </thead>
              <tbody>
                {rows.map((m) => (
                  <tr key={m.id ?? ""} className={selected(m.id) ? "selected" : ""}>
                    <td>
                      {m.id === null ? (
                        <span className="muted">{rowName(shown, m)}</span>
                      ) : (
                        <button type="button" className="link" onClick={() => pick(m)}>
                          {rowName(shown, m)}
                        </button>
                      )}
                    </td>
                    <td className="num">{compact(m.totals.requests)}</td>
                    <td className="num">
                      {m.totals.errors ? `${compact(m.totals.errors)} (${pct(m.totals.errors / m.totals.requests)})` : "0"}
                    </td>
                    <td className="num">{compact(m.totals.input_tokens + m.totals.cache_read_tokens + m.totals.cache_write_tokens)}</td>
                    <td className="num">{compact(m.totals.output_tokens)}</td>
                    <td className="num">{ms(m.totals.avg_latency_ms)}</td>
                    <td className="num">{ms(m.totals.p95_latency_ms)}</td>
                    <td className="num">{ms(m.totals.avg_ttft_ms)}</td>
                    <td className="num">{usd(m.totals.cost_usd)}</td>
                  </tr>
                ))}
                {rows.length === 0 && (
                  <tr>
                    <td colSpan={9} className="empty">
                      No requests in this time range.
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        </>
      )}
    </div>
  );
}
