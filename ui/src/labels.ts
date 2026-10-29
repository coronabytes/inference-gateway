/** Request labels, which clients send as `x-gateway-labels: tenant=…,feature=core`. */
export type Labels = Record<string, string>;

/** A request carries at most this many labels, so a filter has at most this many too. */
export const MAX_LABELS = 5;

/** A label filter as the `labels` query parameter. */
export function labelQuery(labels: Labels): string {
  return Object.entries(labels)
    .map(([k, v]) => `${k}=${v}`)
    .join(",");
}

/** A label filter with `key` set to `value`; unchanged when that would be one label too many. */
export function withLabel(labels: Labels, key: string, value: string): Labels {
  if (!(key in labels) && Object.keys(labels).length >= MAX_LABELS) return labels;
  return { ...labels, [key]: value };
}

/** Why the gateway would refuse this label (see `labels.rs`), or null. */
export function labelError(key: string, value: string): string | null {
  if (!/^[A-Za-z0-9_.-]{1,127}$/.test(key)) return "Label keys are letters, digits, '_', '-' and '.'.";
  if (!/^[\x20-\x7e]{1,127}$/.test(value) || value.includes(",")) return "Label values are printable ASCII without commas.";
  return null;
}

/** A label key of the latest labelled requests, with its values, most used first (`/logs/labels`). */
export interface LabelKey {
  key: string;
  values: { value: string; requests: number }[];
}

/** Headers that send `labels` with a request; none without labels. */
export function labelHeaders(labels: Labels): Record<string, string> {
  const value = labelQuery(labels);
  return value ? { "x-gateway-labels": value } : {};
}

export const LABEL_EDITOR_HINT =
  "Labels sent with each request in the x-gateway-labels header, so the request log and metrics can be filtered by them. Pick a key and then one of its values, both from the latest labelled requests, or type key=value.";

export const LABEL_FILTER_HINT =
  "Requests with all of these labels. Pick a key and then one of its values, both from the latest labelled requests, or type key=value. Clients send labels in the x-gateway-labels header.";
