import type { components } from "./api.gen";

/** The API's types, generated from the gateway's OpenAPI document (`npm run gen:api`). The few
 * written out below are answers the gateway builds as plain JSON, which the document describes only
 * as objects. */
type S = components["schemas"];

const TOKEN_KEY = "gw_admin_token";

export function getToken(): string | null {
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function setToken(token: string | null) {
  try {
    if (token) sessionStorage.setItem(TOKEN_KEY, token);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    /* storage unavailable: token lives only for this page load */
  }
  memoryToken = token;
}

let memoryToken: string | null = getToken();

export class AuthError extends Error {}

/** Headers for every admin request: the master key when signed in with it, and the header the
 * gateway requires on cookie-authenticated requests that change state. */
function adminHeaders(): Record<string, string> {
  return { "x-gateway-csrf": "1", ...(memoryToken ? { authorization: `Bearer ${memoryToken}` } : {}) };
}

/** Raw admin API request (for streaming responses); the caller handles the body. `headers` are
 * sent as well, such as the playground's labels. */
export async function adminFetch(path: string, body: unknown, signal?: AbortSignal, headers: Record<string, string> = {}): Promise<Response> {
  const res = await fetch(`/admin/api${path}`, {
    method: "POST",
    headers: { ...headers, ...adminHeaders(), "content-type": "application/json" },
    body: JSON.stringify(body),
    signal,
  });
  if (res.status === 401 && !res.headers.get("content-type")?.includes("event-stream")) {
    const data = await res.clone().json().catch(() => ({}));
    // 401 from the admin layer (bad master key) signs out; 401 from the pipeline (key disabled) does not.
    if (data?.error === "admin authentication required") window.dispatchEvent(new Event("gw-unauthorized"));
  }
  return res;
}

export async function api<T>(path: string, init: RequestInit & { json?: unknown } = {}): Promise<T> {
  const headers: Record<string, string> = adminHeaders();
  let body = init.body;
  if (init.json !== undefined) {
    headers["content-type"] = "application/json";
    body = JSON.stringify(init.json);
  }
  const res = await fetch(`/admin/api${path}`, { ...init, headers, body });
  if (res.status === 401) {
    window.dispatchEvent(new Event("gw-unauthorized"));
    throw new AuthError("unauthorized");
  }
  if (res.status === 204) return undefined as T;
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data?.error ?? `request failed (${res.status})`);
  return data as T;
}

/** `GET /admin/api/auth/session`: works signed out. */
export type SessionInfo = S["SessionInfo"];

export type OidcSettings = S["OidcSettingsView"];

export type OidcSettingsInput = S["OidcSettingsInput"];

export type AdminUser = S["PersonView"];

export type Provider = S["ProviderKind"];
/** Upstream protocol of a chat deployment. */
export type ApiStyle = S["ApiStyle"];
/** What a deployment serves: chat endpoints, /v1/embeddings, /v1/decisions and /v1/systemone, or /v1/rerank. */
export type Mode = S["Mode"];

export type DeploymentConfig = S["DeploymentConfig"];

export type Deployment = S["DeploymentView"];

/** A deployment's recent calls, each counting half as much a minute later. */
export type DeploymentHealth = S["HealthView"];

export type Secrets = S["Secrets"];

export type CredentialSettings = S["CredentialSettings"];

/** A deployment on an Azure credential's resource, read from Azure Resource Manager (Entra ID credentials only). */
export type FoundryDeployment = S["FoundryDeployment"];

/** An AWS region the gateway's SDK knows (its built-in endpoint partitions). */
export type AwsRegion = S["AwsRegion"];

/** A Bedrock model or inference profile a credential can call, with its account's quota in the credential's region. */
export type BedrockModel = S["BedrockModel"];

export type BedrockModels = S["BedrockModels"];

export type FoundryDeployments = S["FoundryDeployments"];

export type Credential = S["CredentialView"];

export type GuardrailProvider = S["GuardrailProvider"];

/** USD per 1,000 text units (up to 1,000 characters each) for each kind of policy; images per image. */
export type GuardrailPrices = S["GuardrailPrices"];

export type GuardrailConfig = S["GuardrailConfig"];

export type Guardrail = S["GuardrailView"];

/** A guardrail in `GET /guardrails`: with the projects and keys whose every request it screens, by name. */
export type GuardrailListItem = S["GuardrailListItem"];

/** The header in which a request asks for guardrails by name, on top of those enforced on its key. */
export const GUARDRAILS_HEADER = "x-gateway-guardrails";

export type GuardrailInput = S["GuardrailInput"];

export type GuardrailFinding = S["Finding"];

/** What `POST /guardrails/{id}/test` answers. */
export type GuardrailTestResult = S["GuardrailTestResult"];

export type McpAuth = S["McpAuth"];

/** The request labels an `oauth_user` upstream tells a key's sign-ins apart by: one per tenant, or per tenant and user.
 * The API takes any list of labels (`OAuthConfig.sign_in_by`); these are the ones the console offers. */
export type SignInBy = [] | ["tenant"] | ["tenant", "user"];

/** An upstream's OAuth client, for `oauth` (client credentials) and `oauth_user` (each key signs in). */
export type OAuthConfig = S["OAuthConfig"];

/** What `POST /oauth/discover` found. */
export type OAuthDiscovered = S["Discovered"];

/** An MCP client signed in to the gateway with OAuth as a key (`GET /keys/{id}/apps`). */
export type ClientGrant = S["ClientGrant"];

/** An MCP client's sign-in waiting for approval (`GET /connect/{id}`). */
export type ConnectRequest = S["ConnectRequest"];

/** An upstream a key signs in to on its own (`GET /keys/{id}/connections`). */
export type KeyConnection = S["Connection"];

/** One of a key's sign-ins per tenant or user (`GET /keys/{id}/connections/{kind}/{upstream}/sign-ins`). */
export type SignIn = S["SignIn"];

/** An MCP server the gateway proxies at `/mcp/{name}` (`GET /mcp-servers`). */
export type McpServer = S["McpServerView"];

export type McpServerInput = S["McpServerInput"];

/** A tool router route: one MCP server's tools, offered when the decision model finds them needed. */
export type ToolRoute = S["ToolRoute"];

/** A tool router (`GET /tool-routers`): requests name it like an MCP server. */
export type ToolRouter = S["ToolRouterView"];

export type ToolRouterInput = S["ToolRouterInput"];

/** What `POST /mcp-servers/{id}/test` answers. */
export type McpServerTestResult = S["McpServerTestResult"];

/** An A2A agent the gateway proxies at `/a2a/{name}` (`GET /a2a-agents`). */
export type A2aAgent = S["A2aAgentView"];

export type A2aAgentInput = S["A2aAgentInput"];

/** What `POST /a2a-agents/{id}/test` answers: the agent's card, as far as the gateway proxies it. */
export type A2aAgentTestResult = S["A2aAgentTestResult"];

export type CredentialInput = S["CredentialInput"];

/** A tag and how many deployments, routers, MCP servers and keys carry it (`GET /tags`). */
export type TagInfo = S["TagUsage"];

/** A key or project as a picker offers it (`GET /keys/options`, `GET /projects/options`). */
export type NamedItem = S["NamedItem"];

/** What the playground offers for a key (`GET /playground/options`). */
export type PlaygroundOptions = S["PlaygroundOptions"];

export type PlaygroundUpstream = S["PlaygroundUpstream"];

/** Puts a list filter in a query as `a,b`, with `none` (a value that cannot be an id or a tag) sent as `-`. */
export function setListParam(q: URLSearchParams, name: string, values: string[], none?: string) {
  if (values.length) q.set(name, values.map((v) => (v === none ? "-" : v)).join(","));
}

/** Whether a key may use a deployment or router: keys without tags may use all of them, keys
 * with tags those that carry no tag the key lacks. A deployment limited to projects is only for
 * keys in one of them. Mirrors `VirtualKey::allows_deployment`. */
export function keyAllows(key: Pick<VirtualKey, "allowed_tags" | "project_id">, d: { tags: string[]; project_ids?: string[] }): boolean {
  if (d.project_ids?.length && !(key.project_id && d.project_ids.includes(key.project_id))) return false;
  return key.allowed_tags.length === 0 || d.tags.every((t) => key.allowed_tags.includes(t));
}

export type DeploymentInput = S["DeploymentInput"];

export type BudgetPeriod = S["BudgetPeriod"];

/** What a key may call: the inference API, the admin API, or both; `project` is the admin API for
 * the key's project only, with the rights of a user who works in it. */
export type KeyScope = S["KeyScope"];

/** Whether a key with this scope calls models. */
export const callsModels = (s: KeyScope) => s === "inference" || s === "all";

export type BudgetStatus = S["BudgetStatus"];

export type VirtualKey = S["KeyView"];

/** Whether a project's keys' prompts and responses are logged: never, those that opt in, or all. */
export type PromptLogging = S["PromptLogging"];

/** A group of keys. While inactive, its keys are refused like disabled ones; it grants and limits nothing. */
export type Project = S["ProjectView"];

export type ProjectInput = S["ProjectInput"];

export type KeyInput = S["KeyInput"];

// `GET /metrics` builds its answer as plain JSON.

export interface Totals {
  requests: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost_usd: number;
  avg_latency_ms: number | null;
  p95_latency_ms: number | null;
  avg_ttft_ms: number | null;
}

export interface Point {
  ts: number;
  requests: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  avg_latency_ms: number | null;
}

export interface Metrics {
  range: string;
  step_secs: number;
  /** The covered time, end exclusive (ISO timestamps). */
  from: string;
  to: string;
  /** `router`: an auto router, whose totals already include its calls to other models. */
  models: { model: string; active: boolean; router: boolean; totals: Totals }[];
  /** The picked models' totals (every model's except routers' without a pick), with exact percentiles. */
  totals: Totals;
  /** The same, per time bucket. */
  series: Point[];
}

/** Every reasoning effort OpenAI's and Anthropic's APIs take, in order. `none` turns reasoning off (no thinking for Anthropic models). */
export type ReasoningEffort = S["ReasoningEffort"];

export type RouteModel = S["RouteModel"];

/** A named pool of chat models; the decision model picks a route by name, then one of its models serves. */
export type Route = S["Route"];

export type AutoRouter = S["RouterView"];

export type RouterInput = S["RouterInput"];

/** A router in `GET /routers`: with the models it names that no deployment serves. */
export type AutoRouterListItem = S["RouterListItem"];

/** A model name that serves another model: a deployment's or an auto router's, named by its id. */
export type ModelAlias = S["AliasView"];

export type AliasInput = S["AliasInput"];

/** What each guardrail of a logged request was given, found, did and answered. The gateway stores
 * these steps as plain JSON with the request, so the document does not describe them. */
export interface GuardrailStep {
  guardrail: string;
  provider: string;
  action: "passed" | "flagged" | "masked" | "blocked" | "skipped" | "failed";
  duration_ms?: number;
  cost_usd?: number;
  findings?: { policy: string; kind: string; action: string; matched?: string; confidence?: string }[];
  /** The texts it rewrote, by JSON pointer into the request. */
  changes?: { pointer: string; before: string; after: string }[];
  /** Its blocking message. */
  message?: string | null;
  error?: string;
  provider_response?: unknown;
}

/** A logged request's prompt and response (`/logs/{id}/bodies`). */
export type LogBodies = Omit<S["BodiesView"], "guardrails"> & { guardrails: GuardrailStep[] | null };

export type LogRow = S["LogRow"];

/** An audit log entry: who changed what through the admin API. */
export type AuditEntry = S["AuditEntry"];

// `GET /cluster` builds its answer as plain JSON.

export interface Instance {
  id: string;
  version: string;
  started_at: string;
  uptime_secs: number;
  deployments: number;
  keys: number;
  registry_loaded_at: string | null;
  registry_errors: number;
  last_seen: string;
}

export interface Cluster {
  instances: Instance[];
  valkey_ok: boolean;
  postgres_ok: boolean;
  this_instance: string;
  /** The answering instance's settings from its environment; no secrets. */
  settings: InstanceSettings;
}

/** What the instance answering read from its environment at startup. */
export interface InstanceSettings {
  database_max_connections: number;
  valkey_cluster: boolean;
  upstream_allowed_networks: string[];
  master_key: boolean;
  dev_login: boolean;
  cookie_secure: boolean;
  console: boolean;
  listen: string;
}

/** The settings every replica applies (`/settings`), changed in the console without a restart. */
export type GatewaySettings = S["Settings"];

export type GatewaySettingsView = S["SettingsView"];

/** One LiteLLM catalog entry, stored as is (per-token USD prices, limits, capabilities). `GET /prices`
 * builds its answer as plain JSON. */
export interface PriceEntry {
  key: string;
  data: Record<string, unknown>;
}

export type PriceImport = S["PriceImport"];

/** What a manual import changed: with `price_auto_accept` on, also the deployments that took the new prices. */
export type ImportSummary = S["ImportSummary"];

export interface PriceSearch {
  total: number;
  items: PriceEntry[];
  providers: string[];
  modes: string[];
  last_import: PriceImport | null;
  /** When the scheduled import is due; null while it is off. */
  next_import: string | null;
  source: string;
}

/** `GET /admin/api/hub`: the models the gateway serves, or those one key may use. */
export type Hub = Omit<S["HubView"], "models"> & { models: HubModel[] };

export type HubKey = S["HubKey"];

export type HubGuardrail = S["HubGuardrail"];

/** What the hub knows of a model or a deployment; null is unknown. */
export type HubLimits = S["Described"];

/** A model's own description, kept by the gateway. It is free-form JSON (admins may add any field),
 * so the document leaves it open; these are the fields the console reads. */
export interface ModelMetadata {
  display_name?: string;
  /** Who makes the model, such as Anthropic or Google; not who hosts it. */
  vendor?: string;
  description?: string;
  [field: string]: unknown;
}

export type HubModel = Omit<S["HubModel"], "metadata"> & { metadata: ModelMetadata };

/** A model's requests over the last 24 hours. */
export type HubStats = S["HubStats"];

export type HubDeployment = S["HubDeployment"];

/** USD per 1M tokens; `per_k_searches` per 1,000 rerank queries or web searches. */
export type HubPrices = S["HubPrices"];
