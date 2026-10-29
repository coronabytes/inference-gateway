export function compact(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return "–";
  const abs = Math.abs(n);
  if (abs >= 1e9) return `${trim(n / 1e9)}B`;
  if (abs >= 1e6) return `${trim(n / 1e6)}M`;
  if (abs >= 1e3) return `${trim(n / 1e3)}K`;
  return Number.isInteger(n) ? String(n) : n.toFixed(1);
}

function trim(v: number): string {
  return v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1).replace(/\.0$/, "") : v.toFixed(2).replace(/\.?0+$/, "");
}

export function usd(n: number | null | undefined): string {
  if (n == null) return "–";
  if (n === 0) return "$0";
  if (Math.abs(n) < 0.01) return `$${n.toFixed(4)}`;
  if (Math.abs(n) >= 1000) return `$${compact(n)}`;
  return `$${n.toFixed(2)}`;
}

export function ms(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return "–";
  if (n >= 10_000) return `${(n / 1000).toFixed(0)}s`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}s`;
  return `${Math.round(n)}ms`;
}

/** A file size: 820 B, 14 KB, 2.4 MB. */
export function bytes(n: number): string {
  if (n >= 1024 * 1024) return `${trim(n / (1024 * 1024))} MB`;
  if (n >= 1024) return `${trim(n / 1024)} KB`;
  return `${n} B`;
}

export function pct(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return "–";
  if (n === 0) return "0%";
  if (n < 0.001) return "<0.1%";
  return `${(n * 100).toFixed(n < 0.1 ? 1 : 0)}%`;
}

export function timeLabel(tsSecs: number, stepSecs: number): string {
  const d = new Date(tsSecs * 1000);
  if (stepSecs < 3600) return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  // Daily buckets are UTC days.
  if (stepSecs >= 86_400) return d.toLocaleDateString([], { month: "short", day: "numeric", timeZone: "UTC" });
  return d.toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });
}

export function dateTime(iso: string | null | undefined): string {
  if (!iso) return "–";
  return new Date(iso).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
}

export function relative(iso: string): string {
  const s = Math.round((Date.now() - new Date(iso).getTime()) / 1000);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

export function duration(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`;
}

export function until(iso: string): string {
  const s = Math.max(0, Math.round((new Date(iso).getTime() - Date.now()) / 1000));
  if (s < 3600) return `in ${Math.max(1, Math.round(s / 60))}m`;
  if (s < 86400) return `in ${Math.round(s / 3600)}h`;
  return `in ${Math.round(s / 86400)}d`;
}

/** A UTC calendar day as YYYY-MM-DD, `daysAgo` days before today. */
export function utcDay(daysAgo = 0): string {
  return new Date(Date.now() - daysAgo * 86_400_000).toISOString().slice(0, 10);
}

/** A UTC day (YYYY-MM-DD) as "28 Sep", with the year when it is not this year's. */
export function dayLabel(day: string): string {
  const d = new Date(`${day}T00:00:00Z`);
  if (Number.isNaN(d.getTime())) return "…";
  const year = d.getUTCFullYear() !== new Date().getUTCFullYear();
  return d.toLocaleDateString([], { day: "numeric", month: "short", ...(year && { year: "numeric" }), timeZone: "UTC" });
}
