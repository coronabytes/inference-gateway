import { useCallback, useEffect, useMemo, useState } from "react";
import { api, setListParam, type AutoRouter, type AutoRouterListItem, type Credential, type Deployment, type Mode, type ModelAlias, type NamedItem, type TagInfo } from "../api";
import { Banner, Button, MenuButton, MultiSelect, Select, SplitButton, Tabs, TextInput, useDebounced, useHashTab } from "../components/controls";
import { PRICE_FIELDS, deploymentPrices, priceDifferences, type DeploymentPrices } from "../pricing";
import { Badge, ConfirmDialog, EditIcon, ErrorBanner, IconButton, MenuIcon, TokenPrices, type ConfirmRequest } from "../components/ui";
import { compact, ms } from "../format";
import { Credentials } from "./Credentials";
import { DeploymentForm, MODES } from "./DeploymentForm";
import { Guardrails } from "./Guardrails";
import { ModelHub } from "./ModelHub";
import { Playground } from "./Playground";
import { PriceCatalog } from "./PriceCatalog";
import { TagList } from "../components/Tags";
import { RouterCard, RouterForm } from "./Routers";
import { AliasCard, AliasForm } from "./Aliases";

/** The tag filter's value for "no tags"; tags never contain spaces. */
const UNTAGGED = " untagged";
/** The project filter's value for deployments open to every key; project ids never contain spaces. */
const NO_PROJECT = " none";
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

/** A listed deployment as its update takes it: without the fields the server reports but does not
 *  take back, which it refuses as unknown. */
function deploymentInput(d: Deployment) {
  const { id: _id, credential_name: _c, cooldown_secs: _cd, health: _h, reasoning_efforts: _e, created_at: _ca, updated_at: _u, ...input } = d;
  return input;
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

/** The projects a deployment is limited to, by name, for its row. */
function projectNames(ids: string[], projects: NamedItem[]): string[] {
  return ids.map((id) => projects.find((p) => p.id === id)?.name ?? "unknown project").sort((a, b) => a.localeCompare(b));
}

/** Every deployment, router and alias, whatever the list's filters. */
interface AllModels {
  deps: Deployment[];
  routers: AutoRouterListItem[];
  aliases: ModelAlias[];
}

const TABS = ["hub", "playground", "deployments", "guardrails", "credentials", "pricing"] as const;

export function Models() {
  const [deps, setDeps] = useState<Deployment[]>([]);
  const [creds, setCreds] = useState<Credential[] | null>(null);
  const [projects, setProjects] = useState<NamedItem[]>([]);
  const [tags, setTags] = useState<TagInfo[]>([]);
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
  const [routers, setRouters] = useState<AutoRouterListItem[]>([]);
  const [editingRouter, setEditingRouter] = useState<AutoRouter | "new" | null>(null);
  const [routerCopyOf, setRouterCopyOf] = useState<AutoRouter | null>(null);
  const [aliases, setAliases] = useState<ModelAlias[]>([]);
  const [editingAlias, setEditingAlias] = useState<ModelAlias | "new" | null>(null);
  const [aliasCopyOf, setAliasCopyOf] = useState<ModelAlias | null>(null);
  const [priceCheck, setPriceCheck] = useState<PriceCheck | null>(null);
  const [onlyDrift, setOnlyDrift] = useState(false);
  const [checking, setChecking] = useState(false);
  // Every deployment, router and alias, whatever the filters: for the router and alias editors,
  // deleting and the price check. Loaded when first needed, dropped on every reload.
  const [all, setAll] = useState<AllModels | null>(null);

  const search = useDebounced(filter.trim());
  const drifting = onlyDrift && priceCheck ? priceCheck.drift : null;
  // A picked project deleted meanwhile drops out.
  const projectsPicked = projectFilter.filter((p) => p === NO_PROJECT || projects.some((u) => u.id === p));
  // The server filters on the public model name only (not the upstream model), the mode, tags,
  // credentials and projects: it keeps the deployments that carry any picked tag (or none, for
  // "untagged"), use any picked credential or are limited to any picked project (or to none).
  // Routers have no credential and no projects, so a credential filter hides them, and a project
  // filter unless it includes "no project"; aliases have no tags either. "Show only these" after a
  // price check asks for the drifting deployments by id.
  const noOwnFilters = !drifting && !credFilter.length && (!projectsPicked.length || projectsPicked.includes(NO_PROJECT));
  const depQuery = new URLSearchParams();
  if (search) depQuery.set("q", search);
  if (modeFilter !== "all") depQuery.set("mode", modeFilter);
  setListParam(depQuery, "tags", tagFilter, UNTAGGED);
  setListParam(depQuery, "credentials", credFilter);
  setListParam(depQuery, "projects", projectsPicked, NO_PROJECT);
  if (drifting) setListParam(depQuery, "ids", [...drifting.keys()]);
  const routerQuery = new URLSearchParams();
  if (search) routerQuery.set("q", search);
  setListParam(routerQuery, "tags", tagFilter, UNTAGGED);
  const aliasQuery = new URLSearchParams();
  if (search) aliasQuery.set("q", search);
  // Null when the filters leave none of them.
  const listed = (shown: boolean, path: string, q: URLSearchParams) => (shown ? `${path}${q.size ? `?${q.toString().replace(/%2C/g, ",")}` : ""}` : null);
  const depPath = listed(modeFilter === "all" || MODES.some((m) => m.value === modeFilter), "/deployments", depQuery);
  const routerPath = listed(noOwnFilters && (modeFilter === "all" || modeFilter === "router"), "/routers", routerQuery);
  const aliasPath = listed(
    noOwnFilters && (modeFilter === "all" || modeFilter === "alias") && (!tagFilter.length || tagFilter.includes(UNTAGGED)),
    "/aliases",
    aliasQuery,
  );

  const loadList = useCallback(async () => {
    try {
      const [d, r, a] = await Promise.all([
        depPath ? api<Deployment[]>(depPath) : [],
        routerPath ? api<AutoRouterListItem[]>(routerPath) : [],
        aliasPath ? api<ModelAlias[]>(aliasPath) : [],
      ]);
      setDeps(d);
      setRouters(r);
      setAliases(a);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [depPath, routerPath, aliasPath]);

  // What the filters and the deployment editor offer: not reloaded as the filters change.
  const loadContext = useCallback(async () => {
    try {
      const [c, p, t] = await Promise.all([api<Credential[]>("/credentials"), api<NamedItem[]>("/projects/options"), api<TagInfo[]>("/tags")]);
      setCreds(c);
      setProjects(p);
      setTags(t);
      setAll(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  const load = useCallback(async () => {
    await Promise.all([loadList(), loadContext()]);
  }, [loadList, loadContext]);

  // Again on returning from another tab, where credentials or prices may have changed.
  useEffect(() => {
    if (tab === "deployments") loadContext();
  }, [loadContext, tab]);

  useEffect(() => {
    if (tab === "deployments") loadList();
  }, [loadList, tab]);

  const loadAll = async (): Promise<AllModels> => {
    const [d, r, a] = await Promise.all([api<Deployment[]>("/deployments"), api<AutoRouterListItem[]>("/routers"), api<ModelAlias[]>("/aliases")]);
    const every = { deps: d, routers: r, aliases: a };
    setAll(every);
    return every;
  };

  // The router and alias editors pick among every model.
  useEffect(() => {
    if ((editingRouter || editingAlias) && !all) loadAll().catch((e) => setError((e as Error).message));
  }, [editingRouter, editingAlias, all]);

  const groups = useMemo(() => {
    const m = new Map<string, Deployment[]>();
    for (const d of deps) m.set(d.model_name, [...(m.get(d.model_name) ?? []), d]);
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [deps]);

  // Tags deployments and routers carry, not those only keys still hold.
  const knownTags = tags.filter((t) => t.deployments + t.routers > 0).map((t) => t.tag);
  // Only credentials that some deployment uses; their counts also tell whether there are models at all.
  const usedCreds = (creds ?? []).filter((c) => c.deployments > 0).sort((a, b) => a.name.localeCompare(b.name));
  const hasModels = usedCreds.length > 0;
  const filtering = !!search || modeFilter !== "all" || tagFilter.length > 0 || credFilter.length > 0 || projectsPicked.length > 0;
  const shownCount = groups.length + routers.length + aliases.length;

  /** Compare every deployment's prices with its catalog entry, as the deployment form would fill them in. */
  const checkPrices = async () => {
    setChecking(true);
    try {
      const [entries, every] = await Promise.all([
        api<{ deployment_id: string; key: string | null; data: Record<string, unknown> | null }[]>("/prices/check"),
        all ?? loadAll(),
      ]);
      const byId = new Map(every.deps.map((d) => [d.id, d]));
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
      await api(`/deployments/${d.id}`, { method: "PUT", json: { ...deploymentInput(d), ...drift.expected, base_model: d.base_model ?? drift.key } });
      setPriceCheck((pc) => {
        if (!pc) return pc;
        const next = new Map(pc.drift);
        next.delete(d.id);
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

  const askDelete = async (d: Deployment) => {
    // The model's deployments, whatever the filters show.
    let siblings: number;
    try {
      siblings = (all ?? (await loadAll())).deps.filter((x) => x.model_name === d.model_name).length;
    } catch (e) {
      return setError((e as Error).message);
    }
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
  };

  /** Activate or deactivate a deployment, keeping everything else as it is. */
  const setDeploymentEnabled = (d: Deployment, enabled: boolean) =>
    act(() => {
      return api(`/deployments/${d.id}`, { method: "PUT", json: { ...deploymentInput(d), enabled } });
    });

  const setRouterEnabled = (r: AutoRouterListItem, enabled: boolean) =>
    act(() => {
      const { id, created_at: _c, updated_at: _u, missing: _m, ...input } = r;
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
        { value: "playground", label: "Playground" },
        { value: "deployments", label: "Deployments" },
        { value: "guardrails", label: "Guardrails" },
        { value: "credentials", label: "Credentials" },
        { value: "pricing", label: "Pricing" },
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
          {hasModels && (
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
              {projects.length > 0 && (
                <MultiSelect
                  caption="Projects"
                  options={[...projects.map((p) => ({ value: p.id, label: p.name })), { value: NO_PROJECT, label: "No project" }]}
                  values={projectsPicked}
                  onChange={setProjectFilter}
                />
              )}
              <TextInput caption="Model" type="search" value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="filter by name" />
              {filtering && (
                <span className="muted small">
                  {shownCount} {shownCount === 1 ? "model matches" : "models match"}
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
                disabled: !hasModels,
                title: "Routes each request to one of several models; needs a decision deployment",
              },
              {
                label: "Add alias",
                onClick: () => (setAliasCopyOf(null), setEditingAlias("new")),
                disabled: !hasModels,
                title: "Another model name for an existing model",
              },
              { label: checking ? "Checking prices…" : "Check prices", onClick: checkPrices, disabled: checking || !hasModels, title: "Compare prices with the price catalog" },
            ]}
          >
            Add deployment
          </SplitButton>
        </div>
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {priceCheck && (
        <Banner
          tone={priceCheck.drift.size ? "warning" : "good"}
          caption="Price check"
          actions={
            priceCheck.drift.size > 0 && (
              <Button variant="ghost" onClick={() => setOnlyDrift((v) => !v)}>
                {onlyDrift ? "Show all models" : "Show only these"}
              </Button>
            )
          }
          onDismiss={() => (setPriceCheck(null), setOnlyDrift(false))}
        >
          {priceCheck.drift.size
            ? `${priceCheck.drift.size} deployment${priceCheck.drift.size === 1 ? "" : "s"} no longer match the price catalog.`
            : "All prices match the price catalog."}
          {priceCheck.unmatched > 0 && (
            <span className="muted">
              {" "}
              {priceCheck.unmatched} without a catalog entry {priceCheck.unmatched === 1 ? "was" : "were"} not checked.
            </span>
          )}
        </Banner>
      )}
      {creds?.length === 0 && (
        <Banner tone="warning">
          Deployments authenticate with a stored credential. <a href="#/models/credentials">Create a credential</a> first, then add deployments that use it.
        </Banner>
      )}
      {creds && !hasModels && <div className="empty-card">No models yet. Add a deployment to start routing traffic.</div>}
      {hasModels && shownCount === 0 && <div className="empty-card">No model matches the filter.</div>}
      {aliases.map((a) => (
        <AliasCard
          key={a.id}
          alias={a}
          onEdit={() => setEditingAlias(a)}
          onDuplicate={() => (setAliasCopyOf(a), setEditingAlias("new"))}
          onDelete={() => askDeleteAlias(a)}
          onToggle={() => setAliasEnabled(a, !a.enabled)}
        />
      ))}
      {routers.map((r) => (
        <RouterCard
          key={r.id}
          router={r}
          onEdit={() => setEditingRouter(r)}
          onDuplicate={() => (setRouterCopyOf(r), setEditingRouter("new"))}
          onDelete={() => askDeleteRouter(r)}
          onToggle={() => setRouterEnabled(r, !r.enabled)}
        />
      ))}
      {groups.map(([name, list]) => (
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
                    <div className="stat-pairs">
                      <span>{d.weight}</span>
                      <span className="muted small">weight</span>
                      <span>{d.priority}</span>
                      <span className="muted small">priority</span>
                    </div>
                    {reduced(d) && <span className="muted small">now ×{d.health!.weight_factor.toFixed(2)}</span>}
                  </div>
                  <div className="stat optional">
                    <div className="stat-pairs">
                      <span>{d.rpm_limit ? compact(d.rpm_limit) : "–"}</span>
                      <span className="muted small">rpm</span>
                      <span>{d.tpm_limit ? compact(d.tpm_limit) : "–"}</span>
                      <span className="muted small">tpm</span>
                    </div>
                  </div>
                  <div className={`stat${drift ? " price-drift" : ""}`} title={drift ? describeDrift(d, drift) : undefined}>
                    {d.mode === "rerank" ? (
                      <>
                        <span>{d.cost_per_k_searches ?? 0}</span>
                        <span className="muted small">$ per 1K searches</span>
                      </>
                    ) : (
                      <TokenPrices
                        input={d.input_cost_per_mtok}
                        output={d.mode !== "embedding" ? d.output_cost_per_mtok : undefined}
                        long={
                          d.long_context_threshold != null
                            ? { threshold: d.long_context_threshold, input: d.long_input_cost_per_mtok, output: d.long_output_cost_per_mtok }
                            : undefined
                        }
                      />
                    )}
                    {d.mode === "chat" && d.cost_per_k_searches != null && (
                      <span className="muted small">{d.cost_per_k_searches} per 1K web searches</span>
                    )}
                    {drift && (
                      <div className="price-catalog">
                        <span>
                          catalog{" "}
                          {d.mode === "rerank"
                            ? `${drift.expected.cost_per_k_searches ?? 0}`
                            : `${drift.expected.input_cost_per_mtok} · ${drift.expected.output_cost_per_mtok}`}
                          {d.mode === "chat" && drift.fields.includes("cost_per_k_searches") && `, ${drift.expected.cost_per_k_searches ?? 0} per 1K web searches`}
                        </span>
                        <Button className="accept" onClick={() => acceptPrice(d, drift)} aria-label={`Accept catalog prices for ${d.upstream_model}`}>
                          Accept
                        </Button>
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
                        { label: "Delete", onClick: () => askDelete(d), danger: true },
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
                  askDelete(editing);
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
      {editingRouter && all && (
        <RouterForm
          deployments={all.deps}
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
      {editingAlias && all && (
        <AliasForm
          deployments={all.deps}
          routers={all.routers}
          taken={all.aliases.filter((a) => editingAlias === "new" || a.id !== editingAlias.id).map((a) => a.model_name)}
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
