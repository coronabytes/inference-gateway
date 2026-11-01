import { useCallback, useEffect, useState } from "react";
import { api, type Metrics as MetricsData } from "../api";
import { TimeChart } from "../components/TimeChart";
import { MultiSelect, TimeRange, useTimeRange } from "../components/controls";
import { KeyFilter, setKeyFilter } from "../components/KeyFilter";
import { LabelFilter } from "../components/LabelInput";
import { ErrorBanner, StatTile } from "../components/ui";
import { compact, ms, pct, usd } from "../format";
import { labelQuery, type Labels } from "../labels";

const RANGES = ["1h", "24h", "7d", "month", "last_month", "custom"] as const;

const PER: Record<number, string> = { 60: "per minute", 3600: "per hour", 86400: "per day" };

export function Metrics() {
  const time = useTimeRange(RANGES, "month");
  const { range, query: rangeQuery, ended } = time;
  const [modelFilter, setModelFilter] = useState<string[]>([]);
  /** The `KeyFilter`'s choice. */
  const [keyId, setKeyId] = useState("");
  const [labels, setLabels] = useState<Labels>({});
  const labelFilter = labelQuery(labels);
  const models = modelFilter.join(",");
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
      if (models) q.set("models", models);
      setData(await api<MetricsData>(`/metrics?${q}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, [rangeQuery, keyId, labelFilter, models]);

  useEffect(() => {
    load();
    // A custom range that ended before today does not change.
    if (ended) return;
    const t = setInterval(load, range === "1h" ? 15_000 : 60_000);
    return () => clearInterval(t);
  }, [load, range, ended]);

  const toggleModel = (model: string) => setModelFilter((ms) => (ms.includes(model) ? ms.filter((m) => m !== model) : [...ms, model]));

  // The table lists only models with requests in the range, and those picked in the filter,
  // busiest first.
  const usedModels = (data?.models.filter((m) => m.totals.requests > 0 || modelFilter.includes(m.model)) ?? []).sort(
    (a, b) => b.totals.requests - a.totals.requests || a.model.localeCompare(b.model),
  );

  const step = data?.step_secs ?? 60;
  const perUnit = PER[step] ?? "per bucket";
  const series = data?.series ?? [];
  const ts = series.map((p) => p.ts);
  const t = data?.totals;

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
      <ErrorBanner error={error} onDismiss={() => setError(null)} />

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
                      <button type="button" className="link" onClick={() => toggleModel(m.model)}>
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
