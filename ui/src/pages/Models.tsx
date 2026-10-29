import { useCallback, useEffect, useMemo, useState } from "react";
import {
  api,
  type ApiStyle,
  type AutoRouter,
  type Credential,
  type Deployment,
  type DeploymentInput,
  type Mode,
  type ModelAlias,
  type PriceEntry,
  type Project,
  type Provider,
} from "../api";
import { CatalogPicker } from "../components/CatalogPicker";
import { Button, MenuButton, MultiSelect, Segmented, Select, SplitButton, Switch, Tabs, TextInput, useHashTab } from "../components/controls";
import { PRICE_FIELDS, catalogScope, deploymentPrices, priceDifferences, suggestModelName, upstreamFromKey, type DeploymentPrices } from "../pricing";
import { Badge, ConfirmDialog, EditIcon, ErrorBanner, IconButton, MenuIcon, Modal, Section, optInt, optNum, type ConfirmRequest } from "../components/ui";
import { compact, ms } from "../format";
import { PROVIDER_LABEL } from "../providers";
import { Credentials, connectionSummary } from "./Credentials";
import { Guardrails } from "./Guardrails";
import { ModelHub } from "./ModelHub";
import { Playground } from "./Playground";
import { PriceCatalog } from "./PriceCatalog";
import { TagEditor, TagList } from "../components/Tags";
import { RouterCard, RouterForm } from "./Routers";
import { AliasCard, AliasForm } from "./Aliases";

/** Chat protocols per provider. Embedding and decision deployments have one protocol each. */
/** Protocols by provider. The note is what the protocol is for there; it may be cut off in the button. */
const STYLES: Record<Provider, { value: ApiStyle; label: string; note?: string }[]> = {
  openai: [
    { value: "chat", label: "Chat Completions" },
    { value: "responses", label: "Responses" },
  ],
  azure: [
    { value: "chat", label: "Chat Completions", note: "Azure OpenAI" },
    { value: "responses", label: "Responses", note: "Azure OpenAI" },
    { value: "anthropic", label: "Anthropic Messages", note: "Claude in Foundry" },
  ],
  anthropic: [{ value: "anthropic", label: "Anthropic Messages" }],
  bedrock: [
    { value: "converse", label: "Converse", note: "any Bedrock model" },
    { value: "anthropic", label: "Anthropic Messages", note: "Claude via bedrock-mantle" },
  ],
  typesafe: [],
  mistral: [{ value: "chat", label: "Chat Completions" }],
  vertex: [
    { value: "chat", label: "Chat Completions", note: "Gemini and Model Garden" },
    { value: "anthropic", label: "Anthropic Messages", note: "Claude on Vertex AI" },
  ],
  gemini: [{ value: "chat", label: "Chat Completions", note: "OpenAI-compatible" }],
};

/** Providers whose Chat Completions take only `max_tokens`; the gateway sends that. */
const MAX_TOKENS_ONLY: Provider[] = ["mistral", "vertex", "gemini"];

export const MODES: { value: Mode; label: string; endpoints: string }[] = [
  { value: "chat", label: "Chat", endpoints: "/v1/messages, /v1/responses and /v1/chat/completions" },
  { value: "embedding", label: "Embedding", endpoints: "/v1/embeddings" },
  { value: "decision", label: "Decision", endpoints: "/v1/decisions and /v1/systemone" },
  { value: "rerank", label: "Rerank", endpoints: "/v1/rerank" },
];

/** The tag filter's value for "no tags"; tags never contain spaces. */
const UNTAGGED = " untagged";
/** The project filter's value for deployments open to every key; project ids never contain spaces. */
const NO_PROJECT = " none";

/** Providers that serve each mode. */
const MODE_PROVIDERS: Record<Mode, Provider[]> = {
  chat: ["openai", "azure", "bedrock", "anthropic", "vertex", "gemini", "mistral"],
  embedding: ["openai", "azure", "bedrock", "vertex", "gemini", "mistral"],
  decision: ["typesafe", "azure"],
  rerank: ["azure", "bedrock", "openai"],
};

/** List price of TypeSafe Jev and Microsoft-Decision-1: $0.042 per 1M input tokens, output free. */
const DECISION_INPUT_PRICE = "0.042";
/** A deployment whose prices no longer match its catalog entry. */
interface PriceDrift {
  key: string;
  expected: DeploymentPrices;
  fields: (keyof DeploymentPrices)[];
}

interface PriceCheck {
  drift: Map<string, PriceDrift>;
  /** Deployments without a catalog entry to compare with. */
  unmatched: number;
}

const pricesOf = (d: Deployment): DeploymentPrices => ({
  input_cost_per_mtok: d.input_cost_per_mtok,
  output_cost_per_mtok: d.output_cost_per_mtok,
  cache_read_cost_per_mtok: d.cache_read_cost_per_mtok,
  cache_write_cost_per_mtok: d.cache_write_cost_per_mtok,
  long_context_threshold: d.long_context_threshold,
  long_input_cost_per_mtok: d.long_input_cost_per_mtok,
  long_output_cost_per_mtok: d.long_output_cost_per_mtok,
  long_cache_read_cost_per_mtok: d.long_cache_read_cost_per_mtok,
  long_cache_write_cost_per_mtok: d.long_cache_write_cost_per_mtok,
  cost_per_k_searches: d.cost_per_k_searches,
});

/** What changes, for a tooltip: "input 0.1 → 0.15, cache read = input → 0.01". */
function describeDrift(d: Deployment, drift: PriceDrift): string {
  const current = pricesOf(d);
  const show = (v: number | null) => (v === null ? "none" : String(v));
  return drift.fields.map((k) => `${PRICE_FIELDS.find((f) => f.key === k)!.label} ${show(current[k])} → ${show(drift.expected[k])}`).join(", ");
}

/** The projects a deployment is limited to, by name, for its row. */
/** Adaptive routing gives the deployment noticeably less than its weight. */
const reduced = (d: Deployment) => d.enabled && !d.cooldown_secs && !!d.health && d.health.weight_factor < 0.95;

/** A deployment's recent calls for a tooltip, after a space; empty without any. */
function healthText(h: Deployment["health"]): string {
  if (!h) return "";
  const parts = [`${Math.round(h.calls)} recent calls, ${(h.error_rate * 100).toFixed(0)}% failed`];
  if (h.ttft_ms != null) parts.push(`first token after ${ms(h.ttft_ms)} on average`);
  if (h.weight_factor < 0.95) parts.push(`gets ${(h.weight_factor * 100).toFixed(0)}% of its weight`);
  return ` ${parts.join("; ")}.`;
}

function projectNames(ids: string[], projects: Project[]): string[] {
  return ids.map((id) => projects.find((p) => p.id === id)?.name ?? "unknown project").sort((a, b) => a.localeCompare(b));
}

/** List price of Amazon Titan Text Embeddings V2 on Bedrock: $0.02 per 1M input tokens. */
const TITAN_INPUT_PRICE = "0.02";
const TITAN_MODEL = "amazon.titan-embed-text-v2:0";

const TABS = ["hub", "deployments", "guardrails", "credentials", "pricing", "playground"] as const;

export function Models() {
  const [deps, setDeps] = useState<Deployment[]>([]);
  const [creds, setCreds] = useState<Credential[] | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Deployment | "new" | null>(null);
  // The deployment being duplicated while `editing` is "new".
  const [copyOf, setCopyOf] = useState<Deployment | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);
  const [filter, setFilter] = useState("");
  const [modeFilter, setModeFilter] = useState<Mode | "router" | "alias" | "all">("all");
  const [tagFilter, setTagFilter] = useState<string[]>([]);
  const [credFilter, setCredFilter] = useState<string[]>([]);
  const [projectFilter, setProjectFilter] = useState<string[]>([]);
  const [tab, setTab] = useHashTab("models", TABS);
  const [routers, setRouters] = useState<AutoRouter[]>([]);
  const [editingRouter, setEditingRouter] = useState<AutoRouter | "new" | null>(null);
  const [routerCopyOf, setRouterCopyOf] = useState<AutoRouter | null>(null);
  const [aliases, setAliases] = useState<ModelAlias[]>([]);
  const [editingAlias, setEditingAlias] = useState<ModelAlias | "new" | null>(null);
  const [aliasCopyOf, setAliasCopyOf] = useState<ModelAlias | null>(null);
  const [priceCheck, setPriceCheck] = useState<PriceCheck | null>(null);
  const [onlyDrift, setOnlyDrift] = useState(false);
  const [checking, setChecking] = useState(false);

  const load = useCallback(async () => {
    try {
      const [d, c, r, a, p] = await Promise.all([
        api<Deployment[]>("/deployments"),
        api<Credential[]>("/credentials"),
        api<AutoRouter[]>("/routers"),
        api<ModelAlias[]>("/aliases"),
        api<Project[]>("/projects"),
      ]);
      setDeps(d);
      setProjects(p);
      setCreds(c);
      setRouters(r);
      setAliases(a);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  // Again on returning from another tab, where credentials or prices may have changed.
  useEffect(() => {
    if (tab === "deployments") load();
  }, [load, tab]);

  const groups = useMemo(() => {
    const m = new Map<string, Deployment[]>();
    for (const d of deps) m.set(d.model_name, [...(m.get(d.model_name) ?? []), d]);
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [deps]);

  const knownTags = useMemo(() => [...new Set([...deps, ...routers].flatMap((x) => x.tags))].sort(), [deps, routers]);

  // Filters on the public model name only (not the upstream model), the mode, tags, credentials and
  // projects. The tag, credential and project filters keep a model's deployments that carry any
  // picked tag (or none, for "untagged"), use any picked credential or are limited to any picked
  // project (or to none). Routers have no credential and no projects, so a credential filter hides
  // them, and a project filter unless it includes "no project".
  const drifting = onlyDrift && priceCheck ? priceCheck.drift : null;
  const tagged = (tags: string[]) => !tagFilter.length || tagFilter.some((t) => (t === UNTAGGED ? !tags.length : tags.includes(t)));
  // Only projects that some deployment is limited to; a picked one that no longer is drops out.
  const usedProjects = projects.filter((p) => deps.some((d) => d.project_ids.includes(p.id))).sort((a, b) => a.name.localeCompare(b.name));
  const projectsPicked = usedProjects.length ? projectFilter.filter((p) => p === NO_PROJECT || usedProjects.some((u) => u.id === p)) : [];
  const inProject = (ids: string[]) =>
    !projectsPicked.length || projectsPicked.some((p) => (p === NO_PROJECT ? !ids.length : ids.includes(p)));
  const visible = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return groups
      .filter(([name, list]) => name.toLowerCase().includes(q) && (modeFilter === "all" || list[0].mode === modeFilter))
      .map(
        ([name, list]) =>
          [
            name,
            list.filter(
              (d) =>
                tagged(d.tags) &&
                inProject(d.project_ids) &&
                (!credFilter.length || credFilter.includes(d.credential_id)) &&
                (!drifting || drifting.has(d.id)),
            ),
          ] as const,
      )
      .filter(([, list]) => list.length > 0);
  }, [groups, filter, modeFilter, tagFilter, credFilter, projectsPicked.join(), drifting]);
  const visibleRouters = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return drifting || credFilter.length || (projectsPicked.length && !projectsPicked.includes(NO_PROJECT))
      ? []
      : routers.filter((r) => r.model_name.toLowerCase().includes(q) && (modeFilter === "all" || modeFilter === "router") && tagged(r.tags));
  }, [routers, filter, modeFilter, tagFilter, credFilter, projectsPicked.join(), drifting]);
  // Aliases have no tags, credentials or projects of their own; like routers, those filters hide them.
  const visibleAliases = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return drifting || credFilter.length || (projectsPicked.length && !projectsPicked.includes(NO_PROJECT))
      ? []
      : aliases.filter(
          (a) => (a.model_name.toLowerCase().includes(q) || !!a.target?.toLowerCase().includes(q)) && (modeFilter === "all" || modeFilter === "alias") && tagged([]),
        );
  }, [aliases, filter, modeFilter, tagFilter, credFilter, projectsPicked.join(), drifting]);
  // Only credentials that some deployment uses.
  const usedCreds = (creds ?? []).filter((c) => deps.some((d) => d.credential_id === c.id)).sort((a, b) => a.name.localeCompare(b.name));

  /** Compare every deployment's prices with its catalog entry, as the deployment form would fill them in. */
  const checkPrices = async () => {
    setChecking(true);
    try {
      const entries = await api<{ deployment_id: string; key: string | null; data: Record<string, unknown> | null }[]>("/prices/check");
      const byId = new Map(deps.map((d) => [d.id, d]));
      const drift = new Map<string, PriceDrift>();
      let unmatched = 0;
      for (const e of entries) {
        const d = byId.get(e.deployment_id);
        if (!d) continue;
        if (!e.key || !e.data) {
          unmatched++;
          continue;
        }
        const expected = deploymentPrices(e.data, d.mode);
        const fields = priceDifferences(pricesOf(d), expected);
        if (fields.length) drift.set(d.id, { key: e.key, expected, fields });
      }
      setPriceCheck({ drift, unmatched });
      setOnlyDrift(drift.size > 0);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setChecking(false);
    }
  };

  /** Take the catalog's prices for one deployment, and remember its catalog entry. */
  const acceptPrice = (d: Deployment, drift: PriceDrift) =>
    act(async () => {
      const { id, credential_name: _c, cooldown_secs: _cd, health: _h, created_at: _ca, updated_at: _u, ...input } = d;
      await api(`/deployments/${id}`, { method: "PUT", json: { ...input, ...drift.expected, base_model: d.base_model ?? drift.key } });
      setPriceCheck((pc) => {
        if (!pc) return pc;
        const next = new Map(pc.drift);
        next.delete(id);
        if (!next.size) setOnlyDrift(false);
        return { ...pc, drift: next };
      });
    });

  const act = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      await load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const askDelete = (d: Deployment, siblings: number) =>
    setConfirming({
      title: "Delete deployment",
      message: (
        <>
          Delete the <strong>{d.upstream_model}</strong> deployment of <strong>{d.model_name}</strong>?{" "}
          {siblings === 1
            ? `It is the model's only deployment, so clients requesting "${d.model_name}" will get "model not found".`
            : "Traffic for the model moves to its other deployments."}
        </>
      ),
      action: async () => {
        await api(`/deployments/${d.id}`, { method: "DELETE" });
        await load();
      },
    });

  /** Activate or deactivate a deployment, keeping everything else as it is. */
  const setDeploymentEnabled = (d: Deployment, enabled: boolean) =>
    act(() => {
      const { id, credential_name: _c, cooldown_secs: _cd, health: _h, created_at: _ca, updated_at: _u, ...input } = d;
      return api(`/deployments/${id}`, { method: "PUT", json: { ...input, enabled } });
    });

  const setRouterEnabled = (r: AutoRouter, enabled: boolean) =>
    act(() => {
      const { id, created_at: _c, updated_at: _u, ...input } = r;
      return api(`/routers/${id}`, { method: "PUT", json: { ...input, enabled } });
    });

  const askDeleteRouter = (r: AutoRouter) =>
    setConfirming({
      title: "Delete router",
      message: (
        <>
          Delete the router <strong>{r.model_name}</strong>? Clients requesting it will get "model not found". Its routes' models stay.
        </>
      ),
      action: async () => {
        await api(`/routers/${r.id}`, { method: "DELETE" });
        await load();
      },
    });

  const setAliasEnabled = (a: ModelAlias, enabled: boolean) =>
    act(() => api(`/aliases/${a.id}`, { method: "PUT", json: { model_name: a.model_name, target_id: a.target_id, enabled } }));

  const askDeleteAlias = (a: ModelAlias) =>
    setConfirming({
      title: "Delete alias",
      message: (
        <>
          Delete the alias <strong>{a.model_name}</strong>? Clients requesting it will get "model not found". The model it serves stays.
        </>
      ),
      action: async () => {
        await api(`/aliases/${a.id}`, { method: "DELETE" });
        await load();
      },
    });

  const tabs = (
    <Tabs
      label="Models"
      options={[
        { value: "hub", label: "Hub" },
        { value: "deployments", label: "Deployments" },
        { value: "guardrails", label: "Guardrails" },
        { value: "credentials", label: "Credentials" },
        { value: "pricing", label: "Pricing" },
        { value: "playground", label: "Playground" },
      ]}
      value={tab}
      onChange={setTab}
    />
  );

  // The other tabs are pages of their own, shown beneath the tabs.
  if (tab !== "deployments") {
    return (
      <div className="page">
        {tabs}
        {tab === "hub" ? (
          <ModelHub />
        ) : tab === "pricing" ? (
          <PriceCatalog />
        ) : tab === "credentials" ? (
          <Credentials />
        ) : tab === "guardrails" ? (
          <Guardrails />
        ) : (
          <Playground />
        )}
      </div>
    );
  }

  return (
    <div className="page">
      {tabs}
      <div className="page-head">
        <div className="filters">
          {groups.length > 0 && (
            <>
              <Select
                caption="Mode"
                options={[{ value: "all", label: "All" }, ...MODES, { value: "router", label: "Router" }, { value: "alias", label: "Alias" }]}
                value={modeFilter}
                onChange={(m) => setModeFilter(m as Mode | "router" | "alias" | "all")}
              />
              <MultiSelect
                caption="Tags"
                options={[...knownTags.map((t) => ({ value: t, label: t })), { value: UNTAGGED, label: "Untagged" }]}
                values={tagFilter}
                onChange={setTagFilter}
              />
              <MultiSelect
                caption="Credentials"
                options={usedCreds.map((c) => ({ value: c.id, label: c.name }))}
                values={credFilter}
                onChange={setCredFilter}
              />
              {usedProjects.length > 0 && (
                <MultiSelect
                  caption="Projects"
                  options={[...usedProjects.map((p) => ({ value: p.id, label: p.name })), { value: NO_PROJECT, label: "No project" }]}
                  values={projectsPicked}
                  onChange={setProjectFilter}
                />
              )}
              <TextInput caption="Model" type="search" value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="filter by name" />
              {(filter.trim() || modeFilter !== "all" || tagFilter.length > 0 || credFilter.length > 0 || projectsPicked.length > 0) && (
                <span className="muted small">
                  {visible.length + visibleRouters.length + visibleAliases.length} of {groups.length + routers.length + aliases.length} models
                </span>
              )}
            </>
          )}
        </div>
        <div className="page-actions">
          <SplitButton
            variant="primary"
            onClick={() => (setCopyOf(null), setEditing("new"))}
            disabled={!creds?.length}
            menuLabel="More ways to add"
            items={[
              {
                label: "Add router",
                onClick: () => (setRouterCopyOf(null), setEditingRouter("new")),
                disabled: !deps.some((d) => d.mode === "decision"),
                title: "Routes each request to one of several models; needs a decision deployment",
              },
              {
                label: "Add alias",
                onClick: () => (setAliasCopyOf(null), setEditingAlias("new")),
                disabled: !deps.length,
                title: "Another model name for an existing model",
              },
              { label: checking ? "Checking prices…" : "Check prices", onClick: checkPrices, disabled: checking || !deps.length, title: "Compare prices with the price catalog" },
            ]}
          >
            Add deployment
          </SplitButton>
        </div>
      </div>
      <ErrorBanner error={error} />
      {priceCheck && (
        <div className={`banner ${priceCheck.drift.size ? "warn" : "good"}`} role="status">
          <span>
            {priceCheck.drift.size
              ? `${priceCheck.drift.size} deployment${priceCheck.drift.size === 1 ? "" : "s"} no longer match the price catalog.`
              : "All prices match the price catalog."}
            {priceCheck.unmatched > 0 && (
              <span className="muted">
                {" "}
                {priceCheck.unmatched} without a catalog entry {priceCheck.unmatched === 1 ? "was" : "were"} not checked.
              </span>
            )}
          </span>
          <span className="banner-actions">
            {priceCheck.drift.size > 0 && (
              <button className="ghost" onClick={() => setOnlyDrift((v) => !v)}>
                {onlyDrift ? "Show all models" : "Show only these"}
              </button>
            )}
            <button className="ghost" onClick={() => (setPriceCheck(null), setOnlyDrift(false))}>
              Dismiss
            </button>
          </span>
        </div>
      )}
      {creds?.length === 0 && (
        <div className="banner">
          Deployments authenticate with a stored credential. <a href="#/models/credentials">Create a credential</a> first, then add deployments that use it.
        </div>
      )}
      {groups.length === 0 && <div className="empty-card">No models yet. Add a deployment to start routing traffic.</div>}
      {groups.length > 0 && visible.length + visibleRouters.length + visibleAliases.length === 0 && (
        <div className="empty-card">No model matches the filter.</div>
      )}
      {visibleAliases.map((a) => (
        <AliasCard
          key={a.id}
          alias={a}
          onEdit={() => setEditingAlias(a)}
          onDuplicate={() => (setAliasCopyOf(a), setEditingAlias("new"))}
          onDelete={() => askDeleteAlias(a)}
          onToggle={() => setAliasEnabled(a, !a.enabled)}
        />
      ))}
      {visibleRouters.map((r) => (
        <RouterCard
          key={r.id}
          router={r}
          deployments={deps}
          onEdit={() => setEditingRouter(r)}
          onDuplicate={() => (setRouterCopyOf(r), setEditingRouter("new"))}
          onDelete={() => askDeleteRouter(r)}
          onToggle={() => setRouterEnabled(r, !r.enabled)}
        />
      ))}
      {visible.map(([name, list]) => (
        <section key={name} className="card list-card deployment-group">
          <header className="list-head">
            <div className="list-title">
              <h3>
                {name} <Badge tone="neutral">{list[0].mode}</Badge>
              </h3>
            </div>
          </header>
          {/* A deployment per row, on the same grid in every card, so its parts line up across models. */}
          <ul className="list">
            {list.map((d) => {
              const drift = priceCheck?.drift.get(d.id);
              return (
                <li key={d.id} className="list-item">
                  <div className="dep-upstream" title={d.base_model ? `${d.upstream_model} (base model ${d.base_model})` : d.upstream_model}>
                    <TagList tags={d.tags} projects={projectNames(d.project_ids, projects)} />
                    <div className="mono clip">{d.upstream_model}</div>
                    {d.base_model && d.base_model !== d.upstream_model && <div className="muted small clip">{d.base_model}</div>}
                  </div>
                  <div className="stat" title={d.credential_name ?? undefined}>
                    <span className="clip">{d.credential_name ?? <span className="muted">missing</span>}</span>
                    <span className="muted small">credential</span>
                  </div>
                  <div className="stat optional" title={`Weight spreads traffic; lower priority is tried first.${healthText(d.health)}`}>
                    <span>
                      {d.weight} · {d.priority}
                    </span>
                    <span className="muted small">weight · priority</span>
                    {reduced(d) && <span className="muted small">now ×{d.health!.weight_factor.toFixed(2)}</span>}
                  </div>
                  <div className="stat optional">
                    <span>
                      {d.rpm_limit ? compact(d.rpm_limit) : "–"} · {d.tpm_limit ? compact(d.tpm_limit) : "–"}
                    </span>
                    <span className="muted small">rpm · tpm</span>
                  </div>
                  <div className={`stat${drift ? " price-drift" : ""}`} title={drift ? describeDrift(d, drift) : undefined}>
                    <span>{d.mode === "rerank" ? (d.cost_per_k_searches ?? 0) : `${d.input_cost_per_mtok} · ${d.output_cost_per_mtok}`}</span>
                    <span className="muted small">{d.mode === "rerank" ? "$ per 1K searches" : "$ in · out per 1M"}</span>
                    {d.long_context_threshold != null && (
                      <span className="muted small">
                        above {compact(d.long_context_threshold)}: {d.long_input_cost_per_mtok} · {d.long_output_cost_per_mtok}
                      </span>
                    )}
                    {drift && (
                      <div className="price-catalog">
                        <span>
                          catalog{" "}
                          {d.mode === "rerank"
                            ? (drift.expected.cost_per_k_searches ?? 0)
                            : `${drift.expected.input_cost_per_mtok} · ${drift.expected.output_cost_per_mtok}`}
                        </span>
                        <button className="accept" onClick={() => acceptPrice(d, drift)} aria-label={`Accept catalog prices for ${d.upstream_model}`}>
                          Accept
                        </button>
                      </div>
                    )}
                  </div>
                  <div className="dep-status">
                    {!d.enabled ? (
                      <Badge tone="warning">Inactive</Badge>
                    ) : d.cooldown_secs ? (
                      <>
                        <Badge tone="warning">Cooling down {d.cooldown_secs}s</Badge>
                        <Button
                          variant="ghost"
                          className="cell-action"
                          title="Clear cooldown"
                          onClick={() => act(() => api(`/deployments/${d.id}/clear-cooldown`, { method: "POST" }))}
                        >
                          Clear
                        </Button>
                      </>
                    ) : reduced(d) ? (
                      <span title={`Adaptive routing sends it less traffic.${healthText(d.health)}`}>
                        <Badge tone="warning">Degraded</Badge>
                      </span>
                    ) : (
                      <span title={healthText(d.health).trim() || undefined}>
                        <Badge tone="good">Active</Badge>
                      </span>
                    )}
                  </div>
                  <div className="actions">
                    <IconButton label={`Edit ${d.upstream_model}`} title="Edit" onClick={() => setEditing(d)}>
                      <EditIcon />
                    </IconButton>
                    <MenuButton
                      className="icon-btn"
                      label={`More actions for ${d.upstream_model}`}
                      items={[
                        { label: "Duplicate", onClick: () => (setCopyOf(d), setEditing("new")) },
                        { label: d.enabled ? "Deactivate" : "Activate", onClick: () => setDeploymentEnabled(d, !d.enabled) },
                        { label: "Delete", onClick: () => askDelete(d, list.length), danger: true },
                      ]}
                    >
                      <MenuIcon />
                    </MenuButton>
                  </div>
                </li>
              );
            })}
          </ul>
        </section>
      ))}

      {editing && (
        <DeploymentForm
          credentials={creds ?? []}
          knownTags={knownTags}
          projects={projects}
          initial={editing === "new" ? undefined : editing}
          copyOf={editing === "new" ? (copyOf ?? undefined) : undefined}
          onClose={() => setEditing(null)}
          onDelete={
            editing === "new"
              ? undefined
              : () => {
                  setEditing(null);
                  askDelete(editing, deps.filter((d) => d.model_name === editing.model_name).length);
                }
          }
          onSave={async (input) => {
            if (editing === "new") await api("/deployments", { method: "POST", json: input });
            else await api(`/deployments/${editing.id}`, { method: "PUT", json: input });
            setEditing(null);
            await load();
          }}
        />
      )}
      {editingRouter && (
        <RouterForm
          deployments={deps}
          knownTags={knownTags}
          initial={editingRouter === "new" ? undefined : editingRouter}
          copyOf={editingRouter === "new" ? (routerCopyOf ?? undefined) : undefined}
          onClose={() => setEditingRouter(null)}
          onDelete={
            editingRouter === "new"
              ? undefined
              : () => {
                  setEditingRouter(null);
                  askDeleteRouter(editingRouter);
                }
          }
          onSave={async (input) => {
            if (editingRouter === "new") await api("/routers", { method: "POST", json: input });
            else await api(`/routers/${editingRouter.id}`, { method: "PUT", json: input });
            setEditingRouter(null);
            await load();
          }}
        />
      )}
      {editingAlias && (
        <AliasForm
          deployments={deps}
          routers={routers}
          taken={aliases.filter((a) => editingAlias === "new" || a.id !== editingAlias.id).map((a) => a.model_name)}
          initial={editingAlias === "new" ? undefined : editingAlias}
          copyOf={editingAlias === "new" ? (aliasCopyOf ?? undefined) : undefined}
          onClose={() => setEditingAlias(null)}
          onDelete={
            editingAlias === "new"
              ? undefined
              : () => {
                  setEditingAlias(null);
                  askDeleteAlias(editingAlias);
                }
          }
          onSave={async (input) => {
            if (editingAlias === "new") await api("/aliases", { method: "POST", json: input });
            else await api(`/aliases/${editingAlias.id}`, { method: "PUT", json: input });
            setEditingAlias(null);
            await load();
          }}
        />
      )}
      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
    </div>
  );
}

function DeploymentForm({
  initial,
  copyOf,
  credentials,
  knownTags,
  projects,
  onSave,
  onDelete,
  onClose,
}: {
  initial?: Deployment;
  /** Duplicate: a new deployment that starts with this one's values. */
  copyOf?: Deployment;
  /** Tags other deployments use, offered while typing. */
  knownTags: string[];
  /** Projects the deployment can be limited to. */
  projects: Project[];
  credentials: Credential[];
  onSave: (d: DeploymentInput) => Promise<void>;
  /** Offered when editing: closes the dialog and asks for confirmation. */
  onDelete?: () => void;
  onClose: () => void;
}) {
  // An edited deployment, or one being duplicated: its values start the form. Only `initial` makes
  // it an edit (fixed mode, Save, Delete).
  const from = initial ?? copyOf;
  const [mode, setMode] = useState<Mode>(from?.mode ?? "chat");
  const [provider, setProvider] = useState<Provider>(from?.provider ?? "openai");
  const [modelName, setModelName] = useState(from?.model_name ?? "");
  // While empty, or still as suggested, the model name follows the upstream model.
  const [nameAuto, setNameAuto] = useState(!from);
  const [upstream, setUpstream] = useState(from?.upstream_model ?? "");
  const firstFor = (p: Provider) => credentials.find((c) => c.provider === p)?.id ?? "";
  const [credentialId, setCredentialId] = useState(from?.credential_id ?? firstFor(from?.provider ?? "openai"));
  const [style, setStyle] = useState<ApiStyle>(from?.config.api_style ?? STYLES[from?.provider ?? "openai"][0]?.value ?? "chat");
  const [thinking, setThinking] = useState(from?.config.thinking_mode ?? "adaptive");
  const [defaultMax, setDefaultMax] = useState(from?.config.default_max_tokens?.toString() ?? "");
  const [maxField, setMaxField] = useState(from?.config.max_tokens_field ?? "");
  const [weight, setWeight] = useState(String(from?.weight ?? 1));
  const [priority, setPriority] = useState(String(from?.priority ?? 0));
  const [rpm, setRpm] = useState(from?.rpm_limit?.toString() ?? "");
  const [tpm, setTpm] = useState(from?.tpm_limit?.toString() ?? "");
  const [timeout, setTimeoutSecs] = useState(String(from?.timeout_secs ?? 600));
  const [inCost, setInCost] = useState(String(from?.input_cost_per_mtok ?? 0));
  const [outCost, setOutCost] = useState(String(from?.output_cost_per_mtok ?? 0));
  const [crCost, setCrCost] = useState(from?.cache_read_cost_per_mtok?.toString() ?? "");
  const [cwCost, setCwCost] = useState(from?.cache_write_cost_per_mtok?.toString() ?? "");
  const [longThreshold, setLongThreshold] = useState(from?.long_context_threshold?.toString() ?? "");
  const [longIn, setLongIn] = useState(from?.long_input_cost_per_mtok?.toString() ?? "");
  const [longOut, setLongOut] = useState(from?.long_output_cost_per_mtok?.toString() ?? "");
  const [longCr, setLongCr] = useState(from?.long_cache_read_cost_per_mtok?.toString() ?? "");
  const [longCw, setLongCw] = useState(from?.long_cache_write_cost_per_mtok?.toString() ?? "");
  const [perK, setPerK] = useState(from?.cost_per_k_searches?.toString() ?? "");
  const [baseModel, setBaseModel] = useState(from?.base_model ?? "");
  const [tags, setTags] = useState<string[]>(from?.tags ?? []);
  const [projectIds, setProjectIds] = useState<string[]>(from?.project_ids ?? []);
  // Set by the catalog match, not by the user: a later match may replace it.
  const [baseAuto, setBaseAuto] = useState(false);
  // Prices typed by hand (or an existing deployment's) are never overwritten automatically.
  const [pricesTouched, setPricesTouched] = useState(!!from);
  const [pricingNote, setPricingNote] = useState<string | null>(null);
  // An empty threshold means one price tier, and keeps the long-context fields collapsed.
  const longOn = mode === "chat" && longThreshold.trim() !== "";
  const longIncomplete = longOn && (optInt(longThreshold) == null || optNum(longIn) == null || optNum(longOut) == null);
  const [enabled, setEnabled] = useState(from?.enabled ?? true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const chat = mode === "chat";
  const anthropicStyle = chat && (style === "anthropic" || (provider === "bedrock" && style === "converse"));
  const available = credentials.filter((c) => c.provider === provider);
  const selected = available.find((c) => c.id === credentialId);

  // One line each for the collapsed sections.
  const limits = [rpm.trim() && `${rpm.trim()} RPM`, tpm.trim() && `${tpm.trim()} TPM`].filter(Boolean).join(", ");
  const routingSummary = [
    `weight ${weight.trim() || "–"}`,
    `priority ${priority.trim() || "–"}`,
    limits || "no rate limits",
    timeout.trim() && `${timeout.trim()} s timeout`,
  ]
    .filter(Boolean)
    .join(" · ");
  const price = (v: string) => `$${v.trim() || "0"}`;
  const pricingSummary = [
    mode === "rerank"
      ? `${price(perK)} per 1K searches${optNum(inCost) ? `, ${price(inCost)} in` : ""}`
      : mode === "embedding"
        ? `${price(inCost)} in`
        : `${price(inCost)} in / ${price(outCost)} out`,
    longOn && `long context above ${compact(Number(longThreshold) || 0)}`,
  ]
    .filter(Boolean)
    .join(" · ");

  // List prices for a new deployment that has no price yet.
  const prefill = (m: Mode, p: Provider) => {
    if (from || optNum(inCost) || optNum(outCost)) return;
    if (m === "decision") setInCost(DECISION_INPUT_PRICE);
    if (m === "embedding" && p === "bedrock") setInCost(TITAN_INPUT_PRICE);
  };

  const changeProvider = (p: Provider) => {
    setProvider(p);
    setStyle(STYLES[p][0]?.value ?? "chat");
    setCredentialId(firstFor(p));
    prefill(mode, p);
  };

  const changeMode = (m: Mode) => {
    setMode(m);
    if (!MODE_PROVIDERS[m].includes(provider)) changeProvider(MODE_PROVIDERS[m][0]);
    prefill(m, provider);
  };

  const changeUpstream = (v: string) => {
    setUpstream(v);
    if (nameAuto) setModelName(suggestModelName(v));
  };

  const str = (v: number | null) => (v === null ? "" : String(v));
  /**
   * Take a catalog entry as the base model and fill the prices from it; cache prices stay empty
   * (= input) when they are the input price. A match found automatically (`auto`) leaves prices
   * typed by hand alone.
   */
  const applyCatalog = (entry: PriceEntry, auto: boolean) => {
    // The same conversion the price check uses, so a filled-in deployment always matches it.
    const p = deploymentPrices(entry.data, mode);
    setBaseModel(entry.key);
    setBaseAuto(auto);
    if (auto && pricesTouched) return;
    setInCost(str(p.input_cost_per_mtok));
    setOutCost(str(p.output_cost_per_mtok));
    setCrCost(str(p.cache_read_cost_per_mtok));
    setCwCost(str(p.cache_write_cost_per_mtok));
    setLongThreshold(str(p.long_context_threshold));
    setLongIn(str(p.long_input_cost_per_mtok));
    setLongOut(str(p.long_output_cost_per_mtok));
    setLongCr(str(p.long_cache_read_cost_per_mtok));
    setLongCw(str(p.long_cache_write_cost_per_mtok));
    setPerK(str(p.cost_per_k_searches));
    setPricesTouched(false);
    setPricingNote(`Prices filled from the catalog entry ${entry.key}. Check them before saving.`);
  };

  // A new deployment looks its upstream model up in the catalog and takes the exact match as its
  // base model, unless someone picked one; it fills in the prices unless they were typed by hand.
  useEffect(() => {
    if (initial || (baseModel && !baseAuto) || !upstream.trim()) return;
    const t = setTimeout(async () => {
      try {
        const r = await api<{ exact: PriceEntry | null }>(`/prices/match?provider=${provider}&model=${encodeURIComponent(upstream.trim())}`);
        if (r.exact) applyCatalog(r.exact, true);
        else if (baseAuto) {
          setBaseModel("");
          setBaseAuto(false);
          setPricingNote(null);
        }
      } catch {
        /* the catalog is optional */
      }
    }, 400);
    return () => clearTimeout(t);
  }, [provider, upstream, mode]);

  /** Price inputs: typing marks the prices as the user's own. */
  const priceInput = (set: (v: string) => void) => (e: { target: { value: string } }) => {
    set(e.target.value);
    setPricesTouched(true);
    setPricingNote(null);
  };

  const submit = async () => {
    setBusy(true);
    try {
      await onSave({
        model_name: modelName.trim(),
        mode,
        provider,
        upstream_model: upstream.trim(),
        credential_id: credentialId,
        config: {
          ...(chat && { api_style: style }),
          ...(anthropicStyle && thinking !== "adaptive" && { thinking_mode: thinking }),
          ...(chat && optInt(defaultMax) && { default_max_tokens: optInt(defaultMax)! }),
          ...(chat && maxField && style === "chat" && !MAX_TOKENS_ONLY.includes(provider) && { max_tokens_field: maxField }),
          ...(from?.config.headers && { headers: from.config.headers }),
          ...(from?.config.forward_sampling != null && { forward_sampling: from.config.forward_sampling }),
          ...(from?.config.aws_service && { aws_service: from.config.aws_service }),
        },
        weight: optInt(weight) ?? 1,
        priority: parseInt(priority, 10) || 0,
        rpm_limit: optInt(rpm),
        tpm_limit: optInt(tpm),
        timeout_secs: optInt(timeout) ?? 600,
        input_cost_per_mtok: optNum(inCost) ?? 0,
        output_cost_per_mtok: mode === "embedding" || mode === "rerank" ? 0 : (optNum(outCost) ?? 0),
        cache_read_cost_per_mtok: chat ? optNum(crCost) : null,
        cache_write_cost_per_mtok: chat ? optNum(cwCost) : null,
        long_context_threshold: longOn ? optInt(longThreshold) : null,
        long_input_cost_per_mtok: longOn ? optNum(longIn) : null,
        long_output_cost_per_mtok: longOn ? optNum(longOut) : null,
        long_cache_read_cost_per_mtok: longOn ? optNum(longCr) : null,
        long_cache_write_cost_per_mtok: longOn ? optNum(longCw) : null,
        cost_per_k_searches: mode === "rerank" ? optNum(perK) : null,
        base_model: baseModel.trim() || null,
        tags,
        project_ids: projectIds,
        enabled,
      });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={initial ? "Edit deployment" : "Add deployment"}
      onClose={onClose}
      size="wide"
      footer={
        <>
          {onDelete && (
            <Button variant="danger" onClick={onDelete}>
              Delete
            </Button>
          )}
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy || !modelName.trim() || !upstream.trim() || !available.some((c) => c.id === credentialId) || longIncomplete} onClick={submit}>
            {initial ? "Save" : "Add deployment"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} />
      <div className="form">
        <Segmented
          caption="Mode"
          className="mode-bar"
          // Clients of a model call its mode's endpoints, so the mode is fixed once it exists.
          options={MODES.map((m) => ({ value: m.value, label: m.label, disabled: !!initial && mode !== m.value }))}
          value={mode}
          onChange={changeMode}
          hint={`Served on ${MODES.find((m) => m.value === mode)!.endpoints}.${initial ? " Fixed after creation." : ""}`}
        >
          {/* Like the router's Active toggle; a plain caption, since the Mode caption owns the hint. */}
          <span className="control-caption">Active</span>
          <Switch checked={enabled} onChange={setEnabled} label="Active" />
        </Segmented>
        <div className="row">
          <Select
            caption="Provider"
            value={provider}
            onChange={(p) => changeProvider(p as Provider)}
            options={MODE_PROVIDERS[mode].map((p) => ({
              value: p,
              label: PROVIDER_LABEL[p],
              note: credentials.some((c) => c.provider === p) ? undefined : "no credential yet",
            }))}
          />
          <Select
            caption="Credential"
            value={credentialId}
            required
            onChange={setCredentialId}
            empty="None yet"
            options={available.map((c) => ({ value: c.id, label: c.name }))}
            hint={
              selected
                ? `Connects to ${connectionSummary(selected)}. Endpoint, region and keys are managed on the Credentials tab.`
                : "Endpoint, region and keys are managed on the Credentials tab."
            }
          />
        </div>
        {!available.length && (
          <div className="banner">
            No {PROVIDER_LABEL[provider]} credential yet. <a href="#/models/credentials">Create one</a> first.
          </div>
        )}
        <fieldset>
          <legend>Model</legend>
          {/* Upstream IDs can be long (Bedrock profiles, ARNs): two thirds of the line. */}
          <div className="row wide-first">
            <CatalogPicker
              caption="Upstream"
              required
              autoFocus
              hint={
                provider === "bedrock" && mode === "embedding"
                  ? `Amazon Titan Text Embeddings V2: ${TITAN_MODEL}`
                  : provider === "bedrock" && mode === "rerank"
                    ? "amazon.rerank-v1:0 or cohere.rerank-v3-5:0 (or a model ARN)"
                  : provider === "azure" && mode === "rerank"
                    ? "Your Cohere rerank deployment, e.g. Cohere-rerank-v4.0-pro"
                  : mode === "rerank"
                    ? "A model of a server with Cohere's rerank API, e.g. rerank-v3.5 (Cohere) or BAAI/bge-reranker-v2-m3 (vLLM)"
                  : provider === "bedrock"
                    ? "Model or inference profile ID, e.g. anthropic.claude-opus-5-5"
                  : provider === "typesafe"
                    ? "TypeSafe model, e.g. jev-latest"
                  : provider === "vertex" && mode === "embedding"
                    ? "Google embedding model, e.g. gemini-embedding-001"
                  : provider === "vertex" && style === "anthropic"
                    ? "Claude model with its version, e.g. claude-sonnet-4-5@20250929"
                  : provider === "vertex"
                    ? "Gemini model, e.g. gemini-2.5-flash (sent as google/gemini-2.5-flash), or publisher/model for Model Garden"
                  : provider === "gemini"
                    ? mode === "embedding" ? "e.g. gemini-embedding-001" : "e.g. gemini-2.5-flash"
                  : provider === "mistral"
                    ? mode === "embedding" ? "e.g. mistral-embed" : "e.g. mistral-large-latest"
                    : provider === "azure" && mode === "decision"
                      ? "Your Microsoft-Decision-1 deployment name (Azure does not allow naming it Microsoft-Decision-1)"
                      : provider === "azure"
                        ? "Your Foundry deployment name. A catalog pick only suggests it; change it if your deployment is named differently."
                        : mode === "embedding"
                          ? "The model the provider serves, e.g. text-embedding-3-large"
                          : "The model the provider serves, as its API names it."
              }
              value={upstream}
              onChange={changeUpstream}
              onPick={(entry) => {
                changeUpstream(upstreamFromKey(entry.key, entry.data, provider));
                applyCatalog(entry, false);
              }}
              {...catalogScope(provider, mode)}
              placeholder={provider === "typesafe" ? "e.g. jev-latest" : provider === "bedrock" && mode === "embedding" ? `e.g. ${TITAN_MODEL}` : "search the catalog or type"}
            />
            {chat ? (
              <Select caption="Protocol" value={style} onChange={(s) => setStyle(s as ApiStyle)} options={STYLES[provider]} />
            ) : (
              <TextInput
                caption="Protocol"
                value={
                  mode === "decision"
                    ? "System One"
                    : mode === "rerank"
                      ? provider === "bedrock" ? "Bedrock Rerank API" : "Cohere rerank (v2)"
                    : provider === "bedrock"
                      ? "Titan Embeddings V2"
                      : provider === "vertex"
                        ? "Vertex AI predict"
                        : "OpenAI embeddings"
                }
                hint={provider === "bedrock" && mode === "embedding" ? "Amazon Titan Text Embeddings V2, called with InvokeModel." : undefined}
                disabled
              />
            )}
          </div>
          <div className="row">
            <CatalogPicker
              caption="Base"
              hint={
                <>
                  The price catalog entry for this model (the base model); picking one fills in the prices. Found from the upstream model when the
                  catalog has it{provider === "azure" ? ", e.g. azure/gpt-4o for an Azure deployment named prod-4o" : ""}.
                </>
              }
              placeholder="search the catalog"
              value={baseModel}
              onChange={(v) => {
                setBaseModel(v);
                setBaseAuto(false);
              }}
              onPick={(entry) => applyCatalog(entry, false)}
            />
            <TextInput
              caption="Name"
              hint="The model name clients send as `model`. Follows the upstream model until you change it."
              mono
              value={modelName}
              onChange={(e) => {
                setModelName(e.target.value);
                setNameAuto(e.target.value.trim() === "");
              }}
              required
              placeholder={nameAuto ? "from the upstream model" : `e.g. ${mode === "embedding" ? "text-embedding" : mode === "decision" ? "jev" : mode === "rerank" ? "rerank" : "claude-opus"}`}
            />
          </div>
        </fieldset>

        <TagEditor
          caption="Tags"
          hint={
            <ul className="hint-list">
              <li>
                <strong>Keys with tags</strong> can use this deployment only if they hold every one of its tags.
              </li>
              <li>
                <strong>Keys without tags</strong> can use every deployment not limited to other projects.
              </li>
            </ul>
          }
          value={tags}
          onChange={setTags}
          suggestions={knownTags}
        />

        <MultiSelect
          caption="Projects"
          none="Every key"
          hint={
            <ul className="hint-list">
              <li>
                <strong>With projects</strong>, only keys in one of them can use this deployment, whatever tags other keys hold. Their
                tags still apply.
              </li>
              <li>
                <strong>Without</strong>, keys in any project, or in none, can use it as their tags allow.
              </li>
            </ul>
          }
          options={[
            ...[...projects].sort((a, b) => a.name.localeCompare(b.name)).map((p) => ({ value: p.id, label: p.name })),
            // A project deleted meanwhile stays picked until it is removed here.
            ...projectIds.filter((id) => !projects.some((p) => p.id === id)).map((id) => ({ value: id, label: "unknown project" })),
          ]}
          values={projectIds}
          onChange={setProjectIds}
        />

        <Section title="Routing & limits" summary={routingSummary}>
          <div className="row four">
            <TextInput caption="Weight" inputMode="numeric" value={weight} onChange={(e) => setWeight(e.target.value)} />
            <TextInput caption="Priority" hint="Lower is tried first." inputMode="numeric" value={priority} onChange={(e) => setPriority(e.target.value)} />
            <TextInput caption="RPM limit" inputMode="numeric" value={rpm} onChange={(e) => setRpm(e.target.value)} placeholder="none" />
            <TextInput caption="TPM limit" inputMode="numeric" value={tpm} onChange={(e) => setTpm(e.target.value)} placeholder="none" />
          </div>
          <div className="row four">
            <TextInput caption="Timeout (s)" inputMode="numeric" value={timeout} onChange={(e) => setTimeoutSecs(e.target.value)} />
            {chat && (
              <TextInput caption="Default max tokens" inputMode="numeric" value={defaultMax} onChange={(e) => setDefaultMax(e.target.value)} placeholder="auto" />
            )}
            {anthropicStyle && (
              <Select
                caption="Thinking"
                hint="Adaptive for Claude 4.6+."
                value={thinking}
                onChange={setThinking}
                options={[
                  { value: "adaptive", label: "adaptive + effort" },
                  { value: "budget", label: "budget_tokens", note: "older models" },
                ]}
              />
            )}
            {chat && style === "chat" && !MAX_TOKENS_ONLY.includes(provider) && (
              <Select
                caption="Max tokens field"
                value={maxField}
                onChange={setMaxField}
                options={[
                  { value: "", label: "max_completion_tokens" },
                  { value: "max_tokens", label: "max_tokens" },
                ]}
              />
            )}
          </div>
        </Section>

        <Section title={mode === "rerank" ? "Pricing (USD)" : "Pricing (USD per 1M tokens)"} summary={pricingSummary}>
          {pricingNote && <p className="muted small">{pricingNote}</p>}
          <div className="row four">
            {mode === "rerank" && (
              <TextInput
                caption="Per 1K searches"
                hint="A search is one query over up to 100 documents."
                inputMode="decimal"
                value={perK}
                onChange={priceInput(setPerK)}
                placeholder="0"
              />
            )}
            <TextInput
              caption={mode === "rerank" ? "Input per 1M tokens" : "Input"}
              hint={mode === "rerank" ? "For rerankers billed by token." : undefined}
              inputMode="decimal"
              value={inCost}
              onChange={priceInput(setInCost)}
            />
            {mode !== "embedding" && mode !== "rerank" && (
              <TextInput caption="Output" inputMode="decimal" value={outCost} onChange={priceInput(setOutCost)} />
            )}
            {chat && (
              <>
                <TextInput caption="Cache read" inputMode="decimal" value={crCost} onChange={priceInput(setCrCost)} placeholder="= input" />
                <TextInput caption="Cache write" inputMode="decimal" value={cwCost} onChange={priceInput(setCwCost)} placeholder="= input" />
              </>
            )}
          </div>
          {chat && (
            <TextInput
              caption="Long context above (input tokens)"
              hint={
                longOn
                  ? "Requests whose input, including cached tokens, exceeds this are billed entirely at the long-context prices below."
                  : "Leave empty for a single price tier."
              }
              inputMode="numeric"
              value={longThreshold}
              onChange={priceInput(setLongThreshold)}
              placeholder="off, e.g. 200000"
            />
          )}
          {longOn && (
            <div className="row four">
              <TextInput caption="Long input" inputMode="decimal" value={longIn} onChange={priceInput(setLongIn)} placeholder="required" required />
              <TextInput caption="Long output" inputMode="decimal" value={longOut} onChange={priceInput(setLongOut)} placeholder="required" required />
              <TextInput caption="Long cache read" inputMode="decimal" value={longCr} onChange={priceInput(setLongCr)} placeholder="= long input" />
              <TextInput caption="Long cache write" inputMode="decimal" value={longCw} onChange={priceInput(setLongCw)} placeholder="= long input" />
            </div>
          )}
        </Section>
      </div>
    </Modal>
  );
}
