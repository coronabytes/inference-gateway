import { useCallback, useEffect, useMemo, useState } from "react";
import { api, type Metrics as MetricsData, type Point, type Totals } from "../api";
import { TimeChart } from "../components/TimeChart";
import { MultiSelect, TimeRange, useTimeRange } from "../components/controls";
import { KeyFilter, setKeyFilter } from "../components/KeyFilter";
import { LabelFilter } from "../components/LabelInput";
import { ErrorBanner, StatTile } from "../components/ui";
import { compact, ms, pct, usd } from "../format";
import { labelQuery, type Labels } from "../labels";

const RANGES = ["1h", "24h", "7d", "month", "last_month", "custom"] as const;

const PER: Record<number, string> = { 60: "per minute", 3600: "per hour", 86400: "per day" };

function sumSeries(all: Point[][]): Point[] {
  if (all.length === 0) return [];
  return all[0].map((p, i) => {
    let latSum = 0;
    let latN = 0;
    const out: Point = { ts: p.ts, requests: 0, errors: 0, input_tokens: 0, output_tokens: 0, cost_usd: 0, avg_latency_ms: null };
    for (const s of all) {
      const q = s[i];
      if (!q) continue;
      out.requests += q.requests;
      out.errors += q.errors;
      out.input_tokens += q.input_tokens;
      out.output_tokens += q.output_tokens;
      out.cost_usd += q.cost_usd;
      if (q.avg_latency_ms != null) {
        latSum += q.avg_latency_ms * q.requests;
        latN += q.requests;
      }
    }
    out.avg_latency_ms = latN ? latSum / latN : null;
    return out;
  });
}

/** Several models' totals as one; percentiles do not add up, so they are left out. */
function sumTotals(all: Totals[]): Totals {
  const avg = (f: (t: Totals) => number | null) => {
    const ts = all.filter((t) => f(t) != null && t.requests > 0);
    const n = ts.reduce((s, t) => s + t.requests, 0);
    return n ? ts.reduce((s, t) => s + f(t)! * t.requests, 0) / n : null;
  };
  const sum = (f: (t: Totals) => number) => all.reduce((s, t) => s + f(t), 0);
  return {
    requests: sum((t) => t.requests),
    errors: sum((t) => t.errors),
    input_tokens: sum((t) => t.input_tokens),
    output_tokens: sum((t) => t.output_tokens),
    cache_read_tokens: sum((t) => t.cache_read_tokens),
    cache_write_tokens: sum((t) => t.cache_write_tokens),
    cost_usd: sum((t) => t.cost_usd),
    avg_latency_ms: avg((t) => t.avg_latency_ms),
    p50_latency_ms: null,
    p95_latency_ms: null,
    avg_ttft_ms: avg((t) => t.avg_ttft_ms),
  };
}

export function Metrics() {
  const time = useTimeRange(RANGES, "month");
  const { range, query: rangeQuery, ended } = time;
  const [modelFilter, setModelFilter] = useState<string[]>([]);
  /** The `KeyFilter`'s choice. */
  const [keyId, setKeyId] = useState("");
  const [labels, setLabels] = useState<Labels>({});
  const labelFilter = labelQuery(labels);
  const [data, setData] = useState<MetricsData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      if (rangeQuery === null) return;
      const q = new URLSearchParams(rangeQuery);
      setKeyFilter(q, keyId);
      if (labelFilter) q.set("labels", labelFilter);
      setData(await api<MetricsData>(`/metrics?${q}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, [rangeQuery, keyId, labelFilter]);

  useEffect(() => {
    load();
    // A custom range that ended before today does not change.
    if (ended) return;
    const t = setInterval(load, range === "1h" ? 15_000 : 60_000);
    return () => clearInterval(t);
  }, [load, range, ended]);

  const selected = useMemo(() => {
    if (!data) return null;
    // Routers' totals already include their calls to other models, so "all" leaves them out.
    const models = modelFilter.length ? data.models.filter((m) => modelFilter.includes(m.model)) : data.models.filter((m) => !m.router);
    // The server's overall totals have exact percentiles across models, as has one model's.
    return {
      totals: !modelFilter.length ? data.overall : models.length === 1 ? models[0].totals : sumTotals(models.map((m) => m.totals)),
      series: sumSeries(models.map((m) => m.series)),
    };
  }, [data, modelFilter.join()]);

  const toggleModel = (model: string) => setModelFilter((ms) => (ms.includes(model) ? ms.filter((m) => m !== model) : [...ms, model]));

  // The table lists only models with requests in the range, and those picked in the filter,
  // busiest first.
  const usedModels = (data?.models.filter((m) => m.totals.requests > 0 || modelFilter.includes(m.model)) ?? []).sort(
    (a, b) => b.totals.requests - a.totals.requests || a.model.localeCompare(b.model),
  );

  const step = data?.step_secs ?? 60;
  const perUnit = PER[step] ?? "per bucket";
  const ts = selected?.series.map((p) => p.ts) ?? [];
  const t = selected?.totals;

  return (
    <div className={loading && data ? "page refreshing" : "page"}>
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
      <ErrorBanner error={error} />

      {t && (
        <div className="tiles">
          <StatTile label="Requests" value={compact(t.requests)} hint={`${compact(t.errors)} errors`} />
          <StatTile label="Error rate" value={pct(t.requests ? t.errors / t.requests : null)} />
          <StatTile label="Input tokens" value={compact(t.input_tokens + t.cache_read_tokens + t.cache_write_tokens)} hint={`${compact(t.cache_read_tokens)} cache reads`} />
          <StatTile label="Output tokens" value={compact(t.output_tokens)} />
          <StatTile label="Cost" value={usd(t.cost_usd)} />
          <StatTile label="Latency" value={ms(t.avg_latency_ms)} hint={`p95 ${ms(t.p95_latency_ms)} · TTFT ${ms(t.avg_ttft_ms)}`} />
        </div>
      )}

      {selected && (
        <div className="charts">
          <TimeChart
            kind="bars"
            title="Requests"
            subtitle={perUnit}
            timestamps={ts}
            stepSecs={step}
            format={compact}
            series={[
              { name: "Succeeded", color: "var(--series-1)", values: selected.series.map((p) => p.requests - p.errors) },
              { name: "Failed", color: "var(--status-critical)", values: selected.series.map((p) => p.errors) },
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
              { name: "Input", color: "var(--series-1)", values: selected.series.map((p) => p.input_tokens) },
              { name: "Output", color: "var(--series-2)", values: selected.series.map((p) => p.output_tokens) },
            ]}
          />
          <TimeChart
            kind="line"
            title="Average latency"
            subtitle="end-to-end, including streaming"
            timestamps={ts}
            stepSecs={step}
            format={ms}
            series={[{ name: "Latency", color: "var(--series-1)", values: selected.series.map((p) => p.avg_latency_ms) }]}
          />
          <TimeChart
            kind="line"
            title="Cost"
            subtitle={`USD ${perUnit}`}
            timestamps={ts}
            stepSecs={step}
            format={usd}
            series={[{ name: "Cost", color: "var(--series-1)", values: selected.series.map((p) => p.cost_usd) }]}
          />
        </div>
      )}

      {data && (
        <>
          <h3>By model</h3>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Model</th>
                  <th className="num">Requests</th>
                  <th className="num">Errors</th>
                  <th className="num">Input</th>
                  <th className="num">Output</th>
                  <th className="num">Avg latency</th>
                  <th className="num">p95</th>
                  <th className="num">TTFT</th>
                  <th className="num">Cost</th>
                </tr>
              </thead>
              <tbody>
                {usedModels.map((m) => (
                  <tr key={m.model} className={modelFilter.includes(m.model) ? "selected" : ""}>
                    <td>
                      <button className="link" onClick={() => toggleModel(m.model)}>
                        {m.model}
                      </button>
                    </td>
                    <td className="num">{compact(m.totals.requests)}</td>
                    <td className="num">{m.totals.errors ? `${compact(m.totals.errors)} (${pct(m.totals.errors / m.totals.requests)})` : "0"}</td>
                    <td className="num">{compact(m.totals.input_tokens + m.totals.cache_read_tokens + m.totals.cache_write_tokens)}</td>
                    <td className="num">{compact(m.totals.output_tokens)}</td>
                    <td className="num">{ms(m.totals.avg_latency_ms)}</td>
                    <td className="num">{ms(m.totals.p95_latency_ms)}</td>
                    <td className="num">{ms(m.totals.avg_ttft_ms)}</td>
                    <td className="num">{usd(m.totals.cost_usd)}</td>
                  </tr>
                ))}
                {usedModels.length === 0 && (
                  <tr>
                    <td colSpan={9} className="empty">No requests in this time range.</td>
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
