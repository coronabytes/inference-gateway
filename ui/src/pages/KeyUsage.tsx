/**
 * What an existing key has used, under the limits in its form: requests and tokens per minute over
 * the last week, and its spend per budget period.
 */
import { useState } from "react";
import type { BudgetPeriod, KeyRates, KeySpend } from "../api";
import { Segmented } from "../components/controls";
import { TimeChart } from "../components/TimeChart";
import { ErrorBanner } from "../components/ui";
import { compact, usd } from "../format";
import { useAdminData } from "../useAdminData";

const HEIGHT = 120;
/** Bucket sizes for the axis labels; a month's only needs to be 28 days or more. */
const STEP: Record<BudgetPeriod, number> = { day: 86_400, week: 7 * 86_400, month: 30 * 86_400 };

/**
 * The key's busiest minute in each hour of the last week, with the p95 over its busy minutes as a
 * line: requests or tokens, switched in the chart's header, as their scales differ too much to share an axis.
 */
export function KeyRateChart({ keyId }: { keyId: string }) {
  const rates = useAdminData<KeyRates>(`/keys/${keyId}/rates`);
  const [shown, setShown] = useState<"requests" | "tokens">("requests");
  const r = rates.data;
  const points = r?.series ?? [];
  const ts = points.map((p) => p.ts);
  const values = points.map((p) => (shown === "requests" ? p.rpm : p.tpm));
  const p95 = shown === "requests" ? r?.rpm_p95 : r?.tpm_p95;
  return (
    <>
      <ErrorBanner error={rates.error} onDismiss={() => rates.setError(null)} />
      <TimeChart
        kind="line"
        title="Per minute"
        controls={
          <Segmented
            label="Show per minute"
            value={shown}
            onChange={setShown}
            options={[
              { value: "requests", label: "Requests" },
              { value: "tokens", label: "Tokens" },
            ]}
          />
        }
        subtitle={
          r
            ? r.rpm_p95 == null
              ? "no traffic in the last 7 days"
              : `p95 ${compact(r.rpm_p95)} requests · ${compact(r.tpm_p95)} tokens · last 7 days`
            : undefined
        }
        timestamps={ts}
        stepSecs={3600}
        format={compact}
        height={HEIGHT}
        series={[
          { name: "Busiest minute", color: "var(--series-1)", values },
          ...(p95 != null ? [{ name: "p95", color: "var(--series-2)", values: ts.map(() => p95) }] : []),
        ]}
      />
    </>
  );
}

/** The key's spend per day, week or month since it was created, with the total and the average per finished period. */
export function KeySpendChart({ keyId, period }: { keyId: string; period: BudgetPeriod }) {
  const spend = useAdminData<KeySpend>(`/keys/${keyId}/spend?period=${period}`);
  // The previous period's answer is not shown under the new one's title.
  const s = spend.data?.period === period ? spend.data : null;
  const points = s?.series ?? [];
  const n = points.length;
  return (
    <>
      <ErrorBanner error={spend.error} onDismiss={() => spend.setError(null)} />
      <TimeChart
        kind="bars"
        title={`Spend per ${period}`}
        subtitle={
          s
            ? `${usd(s.total_usd)} over ${n} ${period}${n === 1 ? "" : "s"} · ${s.avg_usd == null ? "no full " + period + " yet" : `${usd(s.avg_usd)} per full ${period}`}`
            : undefined
        }
        timestamps={points.map((p) => p.ts)}
        stepSecs={STEP[period]}
        format={usd}
        height={HEIGHT}
        series={[{ name: "Spend", color: "var(--series-1)", values: points.map((p) => p.cost_usd) }]}
      />
    </>
  );
}
