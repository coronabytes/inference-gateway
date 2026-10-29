/** Reading LiteLLM catalog entries, which price per token, into the gateway's USD per 1M tokens. */

const perM = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? Number((v * 1e6).toPrecision(8)) : null);
const int = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

export interface Tier {
  input: number | null;
  output: number | null;
  cacheRead: number | null;
  cacheWrite: number | null;
}

export interface CatalogPrices extends Tier {
  /** Long-context tier: requests above \`threshold\` input tokens. The lowest tier LiteLLM lists. */
  long: (Tier & { threshold: number }) | null;
  maxInput: number | null;
  maxOutput: number | null;
  provider: string | null;
  mode: string | null;
  /** Rerank models: USD per 1,000 searches (LiteLLM prices them per query). */
  perKSearches: number | null;
}

const LONG_INPUT = /^input_cost_per_token_above_(\d+)k_tokens$/;

export function catalogPrices(data: Record<string, unknown>): CatalogPrices {
  const thresholds = Object.keys(data)
    .map((k) => LONG_INPUT.exec(k)?.[1])
    .filter((k): k is string => !!k)
    .map(Number)
    .sort((a, b) => a - b);
  const k = thresholds[0];
  return {
    input: perM(data.input_cost_per_token),
    output: perM(data.output_cost_per_token),
    cacheRead: perM(data.cache_read_input_token_cost),
    cacheWrite: perM(data.cache_creation_input_token_cost),
    long:
      k === undefined
        ? null
        : {
            threshold: k * 1000,
            input: perM(data[`input_cost_per_token_above_${k}k_tokens`]),
            output: perM(data[`output_cost_per_token_above_${k}k_tokens`]),
            cacheRead: perM(data[`cache_read_input_token_cost_above_${k}k_tokens`]),
            cacheWrite: perM(data[`cache_creation_input_token_cost_above_${k}k_tokens`]),
          },
    maxInput: int(data.max_input_tokens) ?? int(data.max_tokens),
    maxOutput: int(data.max_output_tokens),
    provider: typeof data.litellm_provider === "string" ? data.litellm_provider : null,
    mode: typeof data.mode === "string" ? data.mode : null,
    perKSearches: typeof data.input_cost_per_query === "number" && Number.isFinite(data.input_cost_per_query)
      ? Number((data.input_cost_per_query * 1000).toPrecision(8))
      : null,
  };
}

/** A deployment's price fields, as stored. */
export interface DeploymentPrices {
  input_cost_per_mtok: number;
  output_cost_per_mtok: number;
  cache_read_cost_per_mtok: number | null;
  cache_write_cost_per_mtok: number | null;
  long_context_threshold: number | null;
  long_input_cost_per_mtok: number | null;
  long_output_cost_per_mtok: number | null;
  long_cache_read_cost_per_mtok: number | null;
  long_cache_write_cost_per_mtok: number | null;
  /** Rerank models: USD per 1,000 searches. */
  cost_per_k_searches: number | null;
}

export const PRICE_FIELDS: { key: keyof DeploymentPrices; label: string }[] = [
  { key: "input_cost_per_mtok", label: "input" },
  { key: "output_cost_per_mtok", label: "output" },
  { key: "cache_read_cost_per_mtok", label: "cache read" },
  { key: "cache_write_cost_per_mtok", label: "cache write" },
  { key: "long_context_threshold", label: "long-context threshold" },
  { key: "long_input_cost_per_mtok", label: "long input" },
  { key: "long_output_cost_per_mtok", label: "long output" },
  { key: "long_cache_read_cost_per_mtok", label: "long cache read" },
  { key: "long_cache_write_cost_per_mtok", label: "long cache write" },
  { key: "cost_per_k_searches", label: "per 1K searches" },
];

/**
 * The prices a deployment of `mode` gets from a catalog entry, exactly as the deployment form
 * fills them in: cache prices stay empty when they equal the input price, embedding and rerank
 * models have no output price, only chat models get cache and long-context prices, and only rerank
 * models a price per search.
 */
export function deploymentPrices(data: Record<string, unknown>, mode: string): DeploymentPrices {
  const p = catalogPrices(data);
  const chat = mode === "chat";
  const differs = (v: number | null, base: number | null) => (v !== null && v !== base ? v : null);
  const long = chat && p.long && p.long.input !== null && p.long.output !== null ? p.long : null;
  return {
    input_cost_per_mtok: p.input ?? 0,
    output_cost_per_mtok: mode === "embedding" || mode === "rerank" ? 0 : (p.output ?? 0),
    cache_read_cost_per_mtok: chat ? differs(p.cacheRead, p.input) : null,
    cache_write_cost_per_mtok: chat ? differs(p.cacheWrite, p.input) : null,
    long_context_threshold: long ? long.threshold : null,
    long_input_cost_per_mtok: long ? long.input : null,
    long_output_cost_per_mtok: long ? long.output : null,
    long_cache_read_cost_per_mtok: long ? differs(long.cacheRead, long.input) : null,
    long_cache_write_cost_per_mtok: long ? differs(long.cacheWrite, long.input) : null,
    cost_per_k_searches: mode === "rerank" ? p.perKSearches : null,
  };
}

/** The prices as billed: an empty cache price is the input price of its tier. */
function billed(p: DeploymentPrices): DeploymentPrices {
  const long = p.long_context_threshold !== null;
  return {
    ...p,
    cache_read_cost_per_mtok: p.cache_read_cost_per_mtok ?? p.input_cost_per_mtok,
    cache_write_cost_per_mtok: p.cache_write_cost_per_mtok ?? p.input_cost_per_mtok,
    long_cache_read_cost_per_mtok: long ? (p.long_cache_read_cost_per_mtok ?? p.long_input_cost_per_mtok) : null,
    long_cache_write_cost_per_mtok: long ? (p.long_cache_write_cost_per_mtok ?? p.long_input_cost_per_mtok) : null,
  };
}

/** The fields where two sets of prices bill differently (beyond floating-point noise). */
export function priceDifferences(current: DeploymentPrices, expected: DeploymentPrices): (keyof DeploymentPrices)[] {
  const same = (a: number | null, b: number | null) =>
    a === null || b === null ? a === b : Math.abs(a - b) <= 1e-9 * Math.max(1, Math.abs(a), Math.abs(b));
  const [a, b] = [billed(current), billed(expected)];
  return PRICE_FIELDS.map((f) => f.key).filter((k) => !same(a[k], b[k]));
}

/** A per-1M price for display: "$2.50", "$0.042", "–". */
export function perMillion(v: number | null): string {
  if (v === null) return "–";
  if (v === 0) return "$0";
  return `$${v >= 1 ? v.toFixed(2).replace(/\.00$/, "") : Number(v.toPrecision(3))}`;
}

/** LiteLLM providers and modes that fit a deployment's provider and mode, for suggestions. */
export function catalogScope(provider: string, mode: string): { providers: string[]; modes: string[] } {
  const providers: Record<string, string[]> = {
    openai: ["openai"],
    azure: ["azure", "azure_ai"],
    bedrock: ["bedrock", "bedrock_converse"],
    anthropic: ["anthropic"],
    typesafe: ["typesafe"],
    mistral: ["mistral"],
    gemini: ["gemini"],
    vertex: ["vertex_ai", "vertex_ai-language-models", "vertex_ai-anthropic_models", "vertex_ai-embedding-models", "vertex_ai-text-models"],
  };
  const modes: Record<string, string[]> = { chat: ["chat", "responses"], embedding: ["embedding"], decision: ["evaluation"], rerank: ["rerank"] };
  return { providers: providers[provider] ?? [], modes: modes[mode] ?? [] };
}

/**
 * The upstream model name for a catalog key: without LiteLLM's provider prefix, and for Azure and
 * Bedrock without region path segments (`azure/eu/gpt-4o` is `gpt-4o`). For Azure this is only a
 * suggestion: the upstream is your deployment's name.
 */
/**
 * A model name to offer clients for an upstream model: its last path segment, without Bedrock's
 * region and vendor prefixes and `:revision`, a Vertex `@version`, or a release date (with the
 * `-v1` Bedrock puts after it). `us.anthropic.claude-sonnet-4-5-20250929-v1:0` becomes
 * `claude-sonnet-4-5`; `amazon.titan-embed-text-v2:0` keeps its `v2`.
 */
export function suggestModelName(upstream: string): string {
  return (upstream.trim().split("/").pop() ?? "")
    .replace(/@.*$/, "")
    .replace(/^(us|eu|apac|global|jp|au|ca|us-gov)\./, "")
    .replace(/^(anthropic|amazon|cohere|meta|mistral|ai21|deepseek|openai|qwen|writer)\./, "")
    .replace(/:\d+$/, "")
    .replace(/-20\d{6}(-v\d+)?$/, "")
    .replace(/[^A-Za-z0-9\-_.:/@]/g, "-")
    .slice(0, 128);
}

export function upstreamFromKey(key: string, data: Record<string, unknown>, provider: string): string {
  let name = key;
  const p = typeof data.litellm_provider === "string" ? data.litellm_provider : "";
  if (p && name.startsWith(`${p}/`)) name = name.slice(p.length + 1);
  // Vertex partner models are keyed `vertex_ai/…` under providers such as vertex_ai-anthropic_models.
  if (name.startsWith("vertex_ai/")) name = name.slice("vertex_ai/".length);
  if (provider === "azure" || provider === "bedrock" || provider === "typesafe") name = name.split("/").pop() ?? name;
  return name;
}
