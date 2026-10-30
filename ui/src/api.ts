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
export interface SessionInfo {
  authenticated: boolean;
  via: "master_key" | "virtual_key" | "project_key" | "session" | "dev" | "dev_user" | null;
  user: { id?: string; email?: string | null; name?: string | null } | null;
  /** An admin may do everything; a user only works in `projects`. Null while signed out. */
  role: "admin" | "user" | null;
  /** A user's projects; null for an admin. */
  projects: string[] | null;
  sso: { display_name: string } | null;
  /** Whether GATEWAY_MASTER_KEY is set; without it, single sign-on is the only sign-in. */
  master_key: boolean;
  /** Local development: the sign-in screen offers "Dev Login". */
  dev: boolean;
  /** The git tag the gateway was built from, e.g. `v1.0.0`, or `v1.0.0-16-ge9477e1` between tags. */
  version: string;
  /** Whether the API reference at /docs (and /openapi.json) is served. */
  api_docs: boolean;
}

export interface OidcSettings {
  configured: boolean;
  enabled: boolean;
  display_name: string;
  issuer_url: string;
  client_id: string;
  client_secret_set: boolean;
  public_url: string;
  redirect_uri: string | null;
  /** ID token claim with the user's roles; a dotted path reaches nested claims. */
  roles_claim: string;
  /** The role that grants admin access. */
  allowed_role: string | null;
  /** The role that signs in as a user, who works only in the projects an admin adds them to. */
  user_role: string | null;
  updated_at: string | null;
}

export interface OidcSettingsInput extends Omit<OidcSettings, "configured" | "client_secret_set" | "redirect_uri" | "updated_at"> {
  /** Omitted keeps the stored secret; "" removes it. */
  client_secret?: string;
}

export interface AdminUser {
  id: string;
  issuer: string;
  subject: string;
  email: string | null;
  name: string | null;
  created_at: string;
  last_login_at: string;
  /** From their roles at their last sign-in. */
  role: "admin" | "user";
  /** The projects a user works in; empty for admins, who see every project. */
  projects: string[];
}

export type Provider = "openai" | "azure" | "bedrock" | "anthropic" | "typesafe" | "mistral" | "vertex" | "gemini";
/** Upstream protocol of a chat deployment. */
export type ApiStyle = "chat" | "responses" | "anthropic" | "converse";
/** What a deployment serves: chat endpoints, /v1/embeddings, /v1/decisions and /v1/systemone, or /v1/rerank. */
export type Mode = "chat" | "embedding" | "decision" | "rerank";

export interface DeploymentConfig {
  api_style?: ApiStyle;
  headers?: Record<string, string>;
  max_tokens_field?: string;
  default_max_tokens?: number;
  thinking_mode?: string;
  forward_sampling?: boolean;
  aws_service?: string;
  /** Bedrock with the Anthropic Messages protocol: Claude through InvokeModel, or bedrock-mantle (the default). */
  bedrock_api?: "invoke" | "mantle";
}

export interface Deployment {
  id: string;
  model_name: string;
  mode: Mode;
  /** Price catalog key, e.g. azure/gpt-4o for an Azure deployment with its own name. */
  base_model: string | null;
  /** Permission tags: a key with tags needs all of them to use this deployment. */
  tags: string[];
  /** Projects whose keys alone may use this deployment, whatever their tags; empty = any key. */
  project_ids: string[];
  provider: Provider;
  upstream_model: string;
  config: DeploymentConfig;
  credential_id: string;
  credential_name: string | null;
  weight: number;
  priority: number;
  rpm_limit: number | null;
  tpm_limit: number | null;
  timeout_secs: number;
  input_cost_per_mtok: number;
  output_cost_per_mtok: number;
  cache_read_cost_per_mtok: number | null;
  cache_write_cost_per_mtok: number | null;
  /** Input tokens above which the long_* prices apply to the whole request; null = one price tier. */
  long_context_threshold: number | null;
  long_input_cost_per_mtok: number | null;
  long_output_cost_per_mtok: number | null;
  long_cache_read_cost_per_mtok: number | null;
  long_cache_write_cost_per_mtok: number | null;
  /** USD per 1,000 searches: rerank queries, or the web searches a chat model runs. */
  cost_per_k_searches: number | null;
  enabled: boolean;
  cooldown_secs: number | null;
  /** Its recent calls, as adaptive routing sees them; absent without any. */
  health?: DeploymentHealth | null;
  /** The reasoning efforts it accepts, from its price catalog entry; absent while it is not loaded (disabled). */
  reasoning_efforts?: ReasoningEffort[] | null;
  created_at: string;
  updated_at: string;
}

/** A deployment's recent calls, each counting half as much a minute later. */
export interface DeploymentHealth {
  calls: number;
  /** Share of the calls that failed with a rate limit, server error or timeout. */
  error_rate: number;
  /** Average time to first token of streamed responses. */
  ttft_ms: number | null;
  /** The share of its weight the deployment gets now: 1 when healthy, down to 0.05. */
  weight_factor: number;
}

export interface Secrets {
  api_key?: string;
  aws_access_key_id?: string;
  aws_secret_access_key?: string;
  aws_session_token?: string;
  /** Vertex: the service account key file (JSON). */
  google_service_account?: string;
  /** Azure with Entra ID: the app registration's client secret, instead of an API key. */
  client_secret?: string;
}

export interface CredentialSettings {
  /** openai/anthropic/mistral/gemini: API base; azure: resource endpoint (required); bedrock/vertex: optional endpoint override. */
  base_url?: string;
  /** Azure only. */
  api_version?: string;
  /** Bedrock (falls back to the AWS environment); Mistral: global, eu or us; Vertex: global, eu, us or a region. */
  region?: string;
  /** Vertex only; defaults to the service account's project. */
  project_id?: string;
  /** OpenAI-compatible only: the header the API key is sent in, as is, instead of `Authorization: Bearer`. */
  auth_header?: string;
  /**
   * Azure and Vertex AI: "environment" signs in as the gateway's own identity, storing no secret.
   * Azure: workload identity, AZURE_* variables or managed identity; Vertex AI: Application Default Credentials.
   */
  identity?: "environment";
  /** Azure with Entra ID: the app registration's directory (tenant) ID or domain. */
  tenant_id?: string;
  /** Azure with Entra ID: the app registration's application (client) ID. */
  client_id?: string;
}

/** A deployment on an Azure credential's resource, read from Azure Resource Manager (Entra ID credentials only). */
export interface FoundryDeployment {
  /** The deployment name, sent as the upstream model. */
  name: string;
  /** The model deployed, e.g. gpt-4o. */
  model: string;
  model_version?: string;
  /** The model's vendor, e.g. OpenAI, Anthropic, Cohere. */
  format?: string;
  /** Deployment type, e.g. GlobalStandard. */
  sku?: string;
  /** Data zone deployments: the zone, eu or us, from the resource's region. Their prices are keyed azure/eu/… and azure/us/…. */
  data_zone?: "eu" | "us";
  /** Requests and tokens per minute its quota allows. */
  rpm?: number;
  tpm?: number;
  mode: Mode;
  api_style?: ApiStyle;
  state?: string;
}

/** An AWS region the gateway's SDK knows (its built-in endpoint partitions). */
export interface AwsRegion {
  /** e.g. eu-central-1 */
  id: string;
  /** e.g. Europe (Frankfurt) */
  name: string;
  /** aws, aws-eusc (European Sovereign Cloud), aws-us-gov (GovCloud) or aws-cn (China). */
  partition: string;
}

/** A Bedrock model or inference profile a credential can call, with its account's quota in the credential's region. */
export interface BedrockModel {
  /** The model or inference profile ID, sent as the upstream model. */
  id: string;
  /** Display name, e.g. EU Anthropic Claude Sonnet 4. */
  name: string;
  provider?: string;
  /** The model ID in the region, a geographic inference profile (eu., us., …) or the global one: each has its own quota. */
  kind: "on_demand" | "cross_region" | "global";
  mode: Mode;
  /** Requests and tokens per minute the account's quota allows; shared by everything using the account. */
  rpm?: number;
  tpm?: number;
  legacy: boolean;
}

export interface BedrockModels {
  region: string;
  models: BedrockModel[];
  /** Why the models or inference profiles could not be listed (e.g. a missing permission). */
  models_error?: string;
  /** Why the quotas could not be read. */
  quotas_error?: string;
}

export interface FoundryDeployments {
  /** The resource's Azure Resource Manager ID. */
  resource: string;
  /** The resource's region, e.g. swedencentral. */
  location?: string;
  deployments: FoundryDeployment[];
}

export interface Credential {
  id: string;
  name: string;
  provider: Provider;
  settings: CredentialSettings;
  secrets_set: Record<keyof Secrets, boolean>;
  /** Vertex: who the stored key file authenticates as. */
  service_account?: { client_email: string; project_id: string | null };
  deployments: number;
  created_at: string;
  updated_at: string;
}

export type GuardrailProvider = "bedrock" | "mistral" | "model_armor";

/** USD per 1,000 text units (up to 1,000 characters each) for each kind of policy; images per image. */
export interface GuardrailPrices {
  content: number;
  content_image: number;
  topic: number;
  sensitive_information: number;
  word: number;
  contextual_grounding: number;
  automated_reasoning: number;
  /** Mistral: per million input tokens. */
  tokens: number;
}

export interface GuardrailConfig {
  /** Bedrock: the guardrail's id or ARN. */
  guardrail_identifier?: string;
  /** Bedrock: a version number, or DRAFT. */
  guardrail_version?: string;
  /** Mistral: the moderation model; mistral-moderation-latest when unset. */
  model?: string;
  /** Mistral: the categories that block; Model Armor: the filters that do. Others found are flagged. */
  block?: string[];
  /** Mistral: the score from which a category counts as found; Mistral's own decision when unset. */
  threshold?: number;
  /** Model Armor: the template's id. */
  template?: string;
  /** Model Armor: the template's location, such as europe-west4. */
  location?: string;
  /** Model Armor: the template's project; the service account's when unset. */
  project_id?: string;
}

export interface Guardrail {
  id: string;
  name: string;
  description?: string;
  provider: GuardrailProvider;
  credential_id: string;
  credential_name: string | null;
  config: GuardrailConfig;
  prices: GuardrailPrices;
  created_at: string;
  updated_at: string;
}

/** A guardrail in `GET /guardrails`: with the projects and keys whose every request it screens, by name. */
export interface GuardrailListItem extends Guardrail {
  enforced_on: { projects: string[]; keys: string[] };
}

/** The header in which a request asks for guardrails by name, on top of those enforced on its key. */
export const GUARDRAILS_HEADER = "x-gateway-guardrails";

export interface GuardrailInput {
  name: string;
  description?: string;
  provider?: GuardrailProvider;
  credential_id: string;
  config: GuardrailConfig;
  prices?: GuardrailPrices;
}

export interface GuardrailFinding {
  policy: "content" | "topic" | "word" | "pii" | "regex" | "grounding" | "moderation" | "pi_and_jailbreak" | "rai" | "malicious_uris" | "csam" | "sdp";
  kind: string;
  action: string;
  matched?: string;
  confidence?: string;
}

/** What `POST /guardrails/{id}/test` answers. */
export interface GuardrailTestResult {
  ok: boolean;
  latency_ms: number;
  action?: "passed" | "flagged" | "masked" | "blocked";
  outputs?: string[];
  findings?: GuardrailFinding[];
  usage?: Record<string, number>;
  cost_usd?: number;
  status?: number;
  error?: string;
}

export interface CredentialInput {
  name: string;
  provider?: Provider;
  settings: CredentialSettings;
  /** On update, omitted fields are kept and empty strings clear them. */
  secrets: Secrets;
}

/** A tag and how many deployments, routers and keys carry it (`GET /tags`). */
export interface TagInfo {
  tag: string;
  deployments: number;
  routers: number;
  keys: number;
}

/** A key or project as a picker offers it (`GET /keys/options`, `GET /projects/options`). */
export interface NamedItem {
  id: string;
  name: string;
  /** A key's project. */
  project_id?: string;
}

/** What the playground offers for a key (`GET /playground/options`). */
export interface PlaygroundOptions {
  /** `web_search`: requests may have it search the web (it has a web search, and the key and its project allow it). */
  models: { name: string; mode: Mode; router: boolean; web_search: boolean }[];
  /** Those enforced on the key or its project always apply. */
  guardrails: { name: string; enforced: boolean }[];
}

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

export type DeploymentInput = Omit<Deployment, "id" | "credential_name" | "cooldown_secs" | "health" | "created_at" | "updated_at">;

export type BudgetPeriod = "day" | "week" | "month";

/** What a key may call: the inference API, the admin API, or both; `project` is the admin API for
 * the key's project only, with the rights of a user who works in it. */
export type KeyScope = "inference" | "admin" | "all" | "project";

/** Whether a key with this scope calls models. */
export const callsModels = (s: KeyScope) => s === "inference" || s === "all";

export interface BudgetStatus {
  period: BudgetPeriod;
  limit_usd: number;
  spent_usd: number;
  resets_at: string;
}

export interface VirtualKey {
  id: string;
  name: string;
  key_prefix: string;
  scope: KeyScope;
  /** The project the key is grouped under; null for none. */
  project_id: string | null;
  /** False while the key's project is inactive: the key is then refused. */
  project_active: boolean;
  allowed_models: string[];
  /** Deployment tags the key holds; empty = every deployment. */
  allowed_tags: string[];
  rpm_limit: number | null;
  tpm_limit: number | null;
  /** Budget per `budget_period`; null = unlimited. */
  max_budget_usd: number | null;
  budget_period: BudgetPeriod | null;
  /** Lifetime spend. */
  spend_usd: number;
  spend_reset_at: string | null;
  /** Current budget period and its spend; null for unlimited keys. */
  budget: BudgetStatus | null;
  enabled: boolean;
  expires_at: string | null;
  metadata: Record<string, unknown>;
  /** Guardrails every request of the key goes through, on top of its project's. */
  guardrail_ids: string[];
  /** Requests may have the model search the web, unless the key's project forbids it. */
  web_search: boolean;
  created_at: string;
  updated_at: string;
}

/** A group of keys. While inactive, its keys are refused like disabled ones; it grants and limits nothing. */
export interface Project {
  id: string;
  name: string;
  description: string | null;
  /** While false, the project's keys are refused like disabled keys. */
  enabled: boolean;
  /** Budget per `budget_period`, on top of each key's own; null = unlimited. */
  max_budget_usd: number | null;
  budget_period: BudgetPeriod | null;
  spend_reset_at: string | null;
  /** Current budget period and its spend; null for unlimited projects. */
  budget: BudgetStatus | null;
  /** How many keys it groups. */
  keys: number;
  /** Guardrails every request of the project's keys goes through. */
  guardrail_ids: string[];
  /** While false, no key in the project may have the model search the web, whatever its own setting. */
  web_search: boolean;
  created_at: string;
  updated_at: string;
}

export interface ProjectInput {
  name: string;
  description: string | null;
  enabled: boolean;
  max_budget_usd: number | null;
  budget_period: BudgetPeriod | null;
  guardrail_ids?: string[];
  web_search: boolean;
}

export interface KeyInput {
  name: string;
  scope: KeyScope;
  project_id: string | null;
  allowed_models: string[];
  allowed_tags: string[];
  rpm_limit: number | null;
  tpm_limit: number | null;
  max_budget_usd: number | null;
  budget_period: BudgetPeriod | null;
  expires_at: string | null;
  enabled: boolean;
  /** Admins only; omitted, the key keeps its guardrails. */
  guardrail_ids?: string[];
  web_search: boolean;
}

export interface Totals {
  requests: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost_usd: number;
  avg_latency_ms: number | null;
  p50_latency_ms: number | null;
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
export type ReasoningEffort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";

export interface RouteModel {
  model: string;
  /** Replaces the client's effort on requests to this model; unset keeps it. */
  reasoning_effort?: ReasoningEffort | null;
}

/** A named pool of chat models; the decision model picks a route by name, then one of its models serves. */
export interface Route {
  name: string;
  description: string;
  models: RouteModel[];
}

export interface AutoRouter {
  id: string;
  model_name: string;
  decision_model: string;
  instructions: string | null;
  routes: Route[];
  /** The name of the route used when the decision fails; the first route when null. */
  fallback_route: string | null;
  /** Permission tags, with the same rule as a deployment's. */
  tags: string[];
  enabled: boolean;
  /** Conversations stay on the route and model their first turn was sent to. */
  sticky: boolean;
  created_at: string;
  updated_at: string;
}

export type RouterInput = Omit<AutoRouter, "id" | "created_at" | "updated_at">;

/** A router in `GET /routers`: with the models it names that no deployment serves. */
export interface AutoRouterListItem extends AutoRouter {
  missing: string[];
}

/** A model name that serves another model: a deployment's or an auto router's, named by its id. */
export interface ModelAlias {
  id: string;
  model_name: string;
  /** The deployment or router whose model the alias serves; it follows that model's renames. */
  target_id: string;
  /** The target's model name now; null once the target is deleted. */
  target: string | null;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export type AliasInput = Pick<ModelAlias, "model_name" | "target_id" | "enabled">;

export interface LogRow {
  id: string;
  ts: string;
  key_id: string | null;
  key_name: string | null;
  model_name: string;
  deployment_id: string | null;
  provider: string | null;
  api: string;
  stream: boolean;
  status_code: number;
  latency_ms: number;
  ttft_ms: number | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost_usd: number;
  attempts: number;
  error: string | null;
  /** Set on the calls of a routed request: the id of the router's entry. */
  parent_id: string | null;
  /** The labels the client sent (`x-gateway-labels`). */
  labels: Record<string, string> | null;
  /** What the guardrails did to the prompt; never the text they matched. */
  guardrails: { applied: string[]; action: "passed" | "flagged" | "masked" | "blocked"; findings: string[] } | null;
  /** The guardrails' part of `cost_usd`. */
  guardrail_cost_usd: number;
  /** Searches billed: web searches a chat model ran, or a rerank model's searches. */
  searches: number;
  /** The searches' part of `cost_usd`. */
  search_cost_usd: number;
}

/** An audit log entry: who changed what through the admin API. */
export interface AuditEntry {
  id: string;
  ts: string;
  actor_kind: "master_key" | "virtual_key" | "session" | "dev" | "system";
  actor_id: string | null;
  actor_name: string | null;
  action: string;
  target_type: string;
  target_id: string | null;
  target_name: string | null;
  /** For an update, the changed fields as `{ field: { from, to } }`; for a create, the new object. */
  details: Record<string, unknown>;
}

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
export interface GatewaySettings {
  max_attempts: number;
  cooldown_secs: number;
  adaptive_routing: boolean;
  sticky_routing: boolean;
  reload_secs: number;
  request_log_retention_days: number;
  audit_log_retention_days: number;
  response_ttl_minutes: number;
  conversation_quota_mb: number;
  max_body_mb: number;
  header_timeout_secs: number;
  body_timeout_secs: number;
  max_connections: number;
  api_docs: boolean;
  price_import: boolean;
  price_import_hours: number;
}

export interface GatewaySettingsView {
  settings: GatewaySettings;
  defaults: GatewaySettings;
  updated_at: string | null;
}

/** One LiteLLM catalog entry, stored as is (per-token USD prices, limits, capabilities). */
export interface PriceEntry {
  key: string;
  data: Record<string, unknown>;
}

export interface PriceImport {
  imported_at: string;
  source: string;
  total: number;
  added: number;
  updated: number;
  removed: number;
}

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
export interface Hub {
  key?: HubKey;
  /** Enforced ones (`applied` key or project) first, then those a client may ask for. */
  guardrails: HubGuardrail[];
  models: HubModel[];
  /** Models before the mode, vendor and search filters. */
  total: number;
  /** The modes (`router` for auto routers) and vendors among those, for the filters. */
  modes: string[];
  vendors: string[];
}

export interface HubKey {
  id: string;
  name: string;
  scope: KeyScope;
  project: string | null;
  status: "active" | "disabled" | "expired" | "project inactive" | "no inference";
  allowed_models: string[];
  allowed_tags: string[];
}

export interface HubGuardrail {
  id: string;
  name: string;
  description?: string;
  provider: GuardrailProvider;
  applied: "key" | "project" | "on request";
  prices: GuardrailPrices;
}

/** What the hub knows of a model or a deployment; null is unknown. */
export interface HubLimits {
  max_input_tokens: number | null;
  max_output_tokens: number | null;
  retires: string | null;
  /** Chat models: capabilities by path, such as `image_input` and `thinking.supported`. */
  capabilities: Record<string, boolean | null> | null;
}

/** A model's own description, kept by the gateway. */
export interface ModelMetadata {
  display_name?: string;
  /** Who makes the model, such as Anthropic or Google; not who hosts it. */
  vendor?: string;
  description?: string;
  [field: string]: unknown;
}

export interface HubModel extends HubLimits {
  name: string;
  /** For a model alias, what its target is. */
  kind: "model" | "router";
  mode: Mode;
  metadata: ModelMetadata;
  /** Who makes it: the metadata's vendor, or without one the vendor its names suggest. */
  vendor?: string;
  /** Set when `vendor` is suggested from its names rather than saved. */
  vendor_suggested?: boolean;
  tags: string[];
  /** Set for a model alias: the model name it serves. */
  target?: string;
  /** An auto router's models that the key reaches. */
  routes?: string[];
  /** Lowest priority (tried first) first. */
  deployments: HubDeployment[];
  /** With `stats=true`, when it had requests in the last 24 hours. */
  stats?: HubStats;
}

/** A model's requests over the last 24 hours. */
export interface HubStats {
  requests: number;
  errors: number;
  p50_latency_ms?: number;
  tokens_per_second?: number;
  /** `[requests, errors]` per hour, oldest first; the last is the current hour. */
  hours: [number, number][];
}

export interface HubDeployment extends HubLimits {
  id: string;
  /** The model name it is deployed under; differs from the model's for an auto router. */
  model: string;
  provider: Provider;
  upstream_model: string;
  priority: number;
  weight: number;
  first_choice: boolean;
  prices: HubPrices;
}

/** USD per 1M tokens; `per_k_searches` per 1,000 rerank queries or web searches. */
export interface HubPrices {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  long_context_threshold?: number;
  long_input?: number;
  long_output?: number;
  long_cache_read?: number;
  long_cache_write?: number;
  per_k_searches: number;
}
