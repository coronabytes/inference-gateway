import { useCallback, useEffect, useRef, useState } from "react";
import { api, type Metrics as MetricsData } from "../api";
import { TimeChart } from "../components/TimeChart";
import { MultiSelect, Segmented, Tabs, TimeRange, useHashTab, useTimeRange } from "../components/controls";
import { KeyFilter, setKeyFilter } from "../components/KeyFilter";
import { LabelFilter } from "../components/LabelInput";
import { ErrorBanner, SortHeader, StatTile, type Sort } from "../components/ui";
import { compact, ms, pct, usd } from "../format";
import { labelQuery, type Labels } from "../labels";
import { useSessionState } from "../sessionState";

const RANGES = ["1h", "24h", "7d", "month", "last_month", "custom"] as const;

const PER: Record<number, string> = { 60: "per minute", 3600: "per hour", 86400: "per day" };

/** The table's columns, by the names `/metrics` sorts them by. */
type Column = "name" | "requests" | "errors" | "input" | "output" | "latency" | "p95" | "ttft" | "cost";

/** Whose calls the page shows, by `/metrics`'s `calls`: the tabs. */
const CALLS = ["models", "mcp", "a2a"] as const;
type Calls = (typeof CALLS)[number];

const CALL_TABS: { value: Calls; label: string }[] = [
  { value: "models", label: "Models" },
  { value: "mcp", label: "MCP" },
  { value: "a2a", label: "A2A" },
];

/** What a tab's rows are, ungrouped: the model filter's caption and the first "By" choice. */
const SUBJECT: Record<Calls, string> = { models: "Model", mcp: "MCP server", a2a: "A2A agent" };

/** What the table's rows total, by `/metrics`'s `group`; `model` is the tab's models, servers or agents. */
type Group = "model" | "project" | "key";

const GROUPS: { value: Group; label: string }[] = [
  { value: "model", label: "Model" },
  { value: "key", label: "Key" },
  { value: "project", label: "Project" },
];

/** The columns MCP tool calls and A2A agent messages have no values for. */
const TOKEN_COLUMNS: Column[] = ["input", "output", "ttft"];

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

/** A row's name: a model's, a project's or key's, or what stands for one without a name. */
function rowName(group: Group, row: MetricsData["rows"][number]): string {
  if (group === "model" || row.name) return row.name ?? row.id ?? "";
  if (row.id) return group === "project" ? "Deleted project" : "Deleted key";
  return group === "project" ? "No project" : "No key";
}

export function Metrics() {
  const [calls, setCalls] = useHashTab("metrics", CALLS);
  /** MCP tool calls and A2A agent messages: no tokens. */
  const tools = calls !== "models";
  const time = useTimeRange(RANGES, "month", "metrics");
  const { range, query: rangeQuery, ended } = time;
  // Each tab picks its own models, servers or agents.
  const picks = {
    models: useSessionState<string[]>("metrics.models", []),
    mcp: useSessionState<string[]>("metrics.mcp", []),
    a2a: useSessionState<string[]>("metrics.a2a", []),
  };
  const [modelFilter, setModelFilter] = picks[calls];
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

  /** The time range, key, label and model filters, as `/metrics` takes them. */
  const filterQuery = useCallback(() => {
    if (rangeQuery === null) return null;
    const q = new URLSearchParams(rangeQuery);
    setKeyFilter(q, keyId);
    if (labelFilter) q.set("labels", labelFilter);
    if (models) q.set("models", models);
    return q;
  }, [rangeQuery, keyId, labelFilter, models]);

  /** The latest load; an earlier one that answers later (another tab's) is dropped. */
  const latest = useRef(0);
  const load = useCallback(async () => {
    const id = ++latest.current;
    setLoading(true);
    try {
      const q = filterQuery();
      if (q === null) return;
      if (calls !== "models") q.set("calls", calls);
      q.set("group", group);
      q.set("sort", sort.by);
      q.set("order", sort.desc ? "desc" : "asc");
      const d = await api<MetricsData>(`/metrics?${q}`);
      if (id !== latest.current) return;
      setData(d);
      setError(null);
    } catch (e) {
      if (id === latest.current) setError((e as Error).message);
    } finally {
      if (id === latest.current) setLoading(false);
    }
  }, [filterQuery, calls, group, sort]);

  // Another tab's figures are not shown under this one's columns while it loads.
  const [loadedCalls, setLoadedCalls] = useState(calls);
  if (loadedCalls !== calls) {
    setLoadedCalls(calls);
    setData(null);
    if (calls !== "models" && TOKEN_COLUMNS.includes(sort.by)) setSort({ by: "requests", desc: true });
  }

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

  // The ungrouped rows are the tab's models, MCP servers or A2A agents.
  const groups = GROUPS.map((g) => (g.value === "model" ? { ...g, label: SUBJECT[calls] } : g));

  const step = data?.step_secs ?? 60;
  const perUnit = PER[step] ?? "per bucket";
  const series = data?.series ?? [];
  const ts = series.map((p) => p.ts);
  const t = data?.totals;

  return (
    <div className={loading && data ? "page refreshing" : "page"}>
      <Tabs label="Metrics" options={CALL_TABS} value={calls} onChange={setCalls} />
      <div className="page-head">
        <div className="filters">
          <TimeRange {...time.props} />
          <KeyFilter value={keyId} onChange={setKeyId} />
          <MultiSelect
            caption={SUBJECT[calls]}
            values={modelFilter}
            onChange={setModelFilter}
            options={(data?.models ?? []).map((m) => ({ value: m.model, label: m.model, note: m.active ? undefined : "inactive" }))}
          />
          <LabelFilter value={labels} onChange={setLabels} />
        </div>
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />

      {t && (
        <div className="tiles">
          <StatTile label="Requests" value={compact(t.requests)} hint={`${compact(t.errors)} errors`} />
          <StatTile label="Error rate" value={pct(t.requests ? t.errors / t.requests : null)} />
          {!tools && (
            <StatTile
              label="Input tokens"
              value={compact(t.input_tokens + t.cache_read_tokens + t.cache_write_tokens)}
              hint={`${compact(t.cache_read_tokens)} cache reads`}
            />
          )}
          {!tools && <StatTile label="Output tokens" value={compact(t.output_tokens)} />}
          <StatTile label="Cost" value={usd(t.cost_usd)} />
          <StatTile
            label="Latency"
            value={ms(t.avg_latency_ms)}
            hint={tools ? `p95 ${ms(t.p95_latency_ms)}` : `p95 ${ms(t.p95_latency_ms)} · TTFT ${ms(t.avg_ttft_ms)}`}
          />
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
          {!tools && (
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
          )}
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
            <Segmented caption="By" label="Totals by" options={groups} value={group} onChange={pickGroup} />
          </div>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  {head("name", groups.find((g) => g.value === shown)!.label, false)}
                  {head("requests", "Requests")}
                  {head("errors", "Errors")}
                  {!tools && head("input", "Input")}
                  {!tools && head("output", "Output")}
                  {head("latency", "Avg latency")}
                  {head("p95", "p95")}
                  {!tools && head("ttft", "TTFT")}
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
                    {!tools && (
                      <td className="num">{compact(m.totals.input_tokens + m.totals.cache_read_tokens + m.totals.cache_write_tokens)}</td>
                    )}
                    {!tools && <td className="num">{compact(m.totals.output_tokens)}</td>}
                    <td className="num">{ms(m.totals.avg_latency_ms)}</td>
                    <td className="num">{ms(m.totals.p95_latency_ms)}</td>
                    {!tools && <td className="num">{ms(m.totals.avg_ttft_ms)}</td>}
                    <td className="num">{usd(m.totals.cost_usd)}</td>
                  </tr>
                ))}
                {rows.length === 0 && (
                  <tr>
                    <td colSpan={tools ? 6 : 9} className="empty">
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
