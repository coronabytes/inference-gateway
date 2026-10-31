import { Fragment, useCallback, useEffect, useState, type ReactNode } from "react";
import { api, type Hub, type HubDeployment, type HubModel, type HubPrices, type HubStats, type ModelMetadata, type NamedItem } from "../api";
import { Banner, Button, MenuButton, MultiSelect, Segmented, Select, TextArea, TextInput, useDebounced } from "../components/controls";
import { CurlDialog, type CurlRequest } from "../components/CurlButton";
import { TagList } from "../components/Tags";
import { VendorLogo } from "../components/VendorLogo";
import { Badge, BracesIcon, DetailsIcon, DocumentIcon, EditIcon, ErrorBanner, GlobeIcon, IconButton, ImageIcon, ListIcon, Modal, PlayIcon, ReasoningIcon, TilesIcon, TokenPrices } from "../components/ui";
import { compact, ms } from "../format";
import { PROVIDER_LABEL } from "../providers";
import { xlsx, type Cell } from "../xlsx";
import { GUARDRAIL_LABEL } from "./Guardrails";
import { MODES } from "./Models";

/** Capabilities the hub shows, by path in a model's `capabilities`: an icon, named by its label. */
const CAPABILITIES: { path: string; label: string; icon: ReactNode }[] = [
  { path: "image_input", label: "Images", icon: <ImageIcon /> },
  { path: "pdf_input", label: "PDF", icon: <DocumentIcon /> },
  { path: "thinking.supported", label: "Reasoning", icon: <ReasoningIcon /> },
  { path: "structured_outputs", label: "Structured output", icon: <BracesIcon /> },
  { path: "server_tools.web_search", label: "Web search", icon: <GlobeIcon /> },
];

type Capable = { capabilities: Record<string, boolean | null> | null };
const capabilitiesOf = (m: Capable) => CAPABILITIES.filter((c) => m.capabilities?.[c.path] === true);
/** For the exports, which list them by name. */
const capabilityNames = (m: Capable) => capabilitiesOf(m).map((c) => c.label);

/** A model's capabilities as icons, each named by its tooltip and for screen readers. */
function CapabilityIcons({ m, picked, onPick }: { m: Capable; picked?: string[]; onPick?: (path: string) => void }) {
  const caps = capabilitiesOf(m);
  if (!caps.length) return <span className="muted">–</span>;
  // With `onPick`, each icon is a button that adds its capability to the hub's filter, or takes it out.
  if (onPick) {
    return (
      <span className="cap-icons" role="group" aria-label="Capabilities">
        {caps.map((c) => {
          const on = picked?.includes(c.path) ?? false;
          return (
            <button
              key={c.path}
              type="button"
              className="cap-icon"
              aria-pressed={on}
              aria-label={c.label}
              title={on ? `${c.label}: show models without it too` : `${c.label}: show only models with it`}
              onClick={(e) => {
                e.stopPropagation();
                onPick(c.path);
              }}
            >
              {c.icon}
            </button>
          );
        })}
      </span>
    );
  }
  return (
    <span className="cap-icons" role="list" aria-label="Capabilities">
      {caps.map((c) => (
        <span key={c.path} className="cap-icon" role="listitem" aria-label={c.label} title={c.label}>
          {c.icon}
        </span>
      ))}
    </span>
  );
}

/** A price as typed in a deployment: 0.15, 3, 0.0004. */
const price = (n: number | undefined) => (n == null ? "–" : String(+n.toFixed(6)));

/** One price, or the range of several: "3", "0.8–3". */
function range(values: number[]): string {
  if (!values.length) return "–";
  const [lo, hi] = [Math.min(...values), Math.max(...values)];
  return lo === hi ? price(lo) : `${price(lo)}–${price(hi)}`;
}

/** Deployments that bill the same, the first-choice ones first. */
interface PriceOption {
  prices: HubPrices;
  deployments: HubDeployment[];
  firstChoice: boolean;
}

function priceOptions(m: HubModel): PriceOption[] {
  const byPrice = new Map<string, PriceOption>();
  for (const d of m.deployments) {
    const id = JSON.stringify(d.prices);
    const o = byPrice.get(id) ?? { prices: d.prices, deployments: [], firstChoice: false };
    o.deployments.push(d);
    o.firstChoice ||= d.first_choice;
    byPrice.set(id, o);
  }
  return [...byPrice.values()].sort((a, b) => Number(b.firstChoice) - Number(a.firstChoice) || a.prices.input - b.prices.input || a.prices.output - b.prices.output);
}

/** Rerank models bill searches; chat models may bill the web searches they run. */
const searches = (m: HubModel) => m.mode === "rerank" || (m.mode === "chat" && m.deployments.some((d) => d.prices.per_k_searches > 0));

/** The headline price of a model: what its deployments charge, as a range where they differ. Long-context
 *  prices show when all its deployments switch to them at the same threshold. */
function PriceSummary({ model: m }: { model: HubModel }) {
  const ps = m.deployments.map((d) => d.prices);
  if (m.mode === "rerank") {
    return (
      <>
        <div>{range(ps.map((p) => p.per_k_searches))}</div>
        <div className="muted small">$ per 1K searches</div>
      </>
    );
  }
  const thresholds = new Set(ps.map((p) => p.long_context_threshold));
  const threshold = thresholds.size === 1 ? ps[0].long_context_threshold : undefined;
  const embedding = m.mode === "embedding";
  return (
    <TokenPrices
      input={range(ps.map((p) => p.input))}
      output={embedding ? undefined : range(ps.map((p) => p.output))}
      long={
        threshold != null
          ? {
              threshold,
              input: range(ps.map((p) => p.long_input ?? p.input)),
              output: embedding ? undefined : range(ps.map((p) => p.long_output ?? p.output)),
            }
          : undefined
      }
    />
  );
}

/** The vendor filter's value for models without one; vendors are trimmed, so never this. */
const NO_VENDOR = " none";

const displayName = (m: HubModel) => m.metadata.display_name?.trim() || m.name;

/** Vendors offered in the editor; others can be typed. */
const VENDORS = ["Alibaba", "Amazon", "Anthropic", "Cohere", "DeepSeek", "Google", "Meta", "Microsoft", "Mistral AI", "Moonshot AI", "OpenAI", "TypeSafe", "xAI", "Zhipu AI"];

const STATUS_TEXT: Record<NonNullable<Hub["key"]>["status"], string> = {
  active: "",
  disabled: "This key is disabled: its requests are refused.",
  expired: "This key has expired: its requests are refused.",
  "project inactive": "This key's project is inactive: its requests are refused.",
  "no inference": "This key's scope is the admin API only: it cannot call these models.",
};

function save(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}

type View = "list" | "tiles";
const VIEW_KEY = "gateway-hub-view";

/** The view picked last in this browser; tiles to start with. */
function initialView(): View {
  try {
    return localStorage.getItem(VIEW_KEY) === "list" ? "list" : "tiles";
  } catch {
    return "tiles";
  }
}

/** A file name's part for a key: "Team A (prod)" → "team-a-prod". */
const slug = (s: string) => s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") || "key";

/** Opens the playground on `model`, as `key` if it may call the model, else as another key that may. */
function openPlayground(model: string, key: string) {
  const params = new URLSearchParams({ model });
  if (key) params.set("key", key);
  window.location.hash = `#/models/playground?${params}`;
}

/** The play button of a model's row or tile. */
function PlayButton({ model, keyId }: { model: string; keyId: string }) {
  return (
    <IconButton label={`Try ${model} in the playground`} title="Try in the playground" onClick={() => openPlayground(model, keyId)}>
      <PlayIcon />
    </IconButton>
  );
}

/**
 * The model hub: every model the gateway serves, or with a key picked those that key may use, with
 * token limits, capabilities, the prices of the deployments that may serve each one, and the
 * guardrails the key's requests go through. Each model's display name and description are edited
 * here.
 */
export function ModelHub() {
  const [keys, setKeys] = useState<NamedItem[]>([]);
  const [keyId, setKeyId] = useState("");
  const [hub, setHub] = useState<Hub | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [modeFilter, setModeFilter] = useState<string[]>([]);
  const [q, setQ] = useState("");
  const [vendorFilter, setVendorFilter] = useState<string[]>([]);
  // Capabilities by path; a model must have all of them.
  const [capFilter, setCapFilter] = useState<string[]>([]);
  const toggleCap = (path: string) => setCapFilter((cs) => (cs.includes(path) ? cs.filter((c) => c !== path) : [...cs, path]));
  const [open, setOpen] = useState<string | null>(null);
  const [editing, setEditing] = useState<HubModel | null>(null);
  const [curl, setCurl] = useState<CurlRequest | null>(null);
  const [view, setView] = useState<View>(initialView);

  const pickView = (v: View) => {
    try {
      localStorage.setItem(VIEW_KEY, v);
    } catch {
      // Not remembered, but still switched for this visit.
    }
    setView(v);
  };

  useEffect(() => {
    api<NamedItem[]>("/keys/options")
      .then(setKeys)
      .catch((e) => setError((e as Error).message));
  }, []);

  // The modes and vendors among the key's models, whatever the filters pick.
  const knownModes = new Set(hub?.modes ?? []);
  const knownVendors = hub?.vendors ?? [];
  // A picked mode or vendor no models have any more drops out.
  const modes = hub ? modeFilter.filter((m) => knownModes.has(m)) : modeFilter;
  const vendors = hub ? vendorFilter.filter((v) => v === NO_VENDOR || knownVendors.includes(v)) : vendorFilter;
  const search = useDebounced(q.trim());

  /** The hub's query: the key and the filters picked here. */
  const hubQuery = (term: string) => {
    const params = new URLSearchParams();
    if (keyId) params.set("key", keyId);
    if (modes.length) params.set("mode", modes.join(","));
    if (vendors.length) params.set("vendor", vendors.map((v) => (v === NO_VENDOR ? "-" : v)).join(","));
    if (capFilter.length) params.set("capability", capFilter.join(","));
    if (term) params.set("q", term);
    return params.toString().replace(/%2C/g, ",");
  };
  const query = hubQuery(search);
  const tiles = view === "tiles";

  const load = useCallback(async () => {
    try {
      // Both views show each model's last 24 hours from the request log.
      setHub(await api<Hub>(`/hub?${[query, "stats=true"].filter(Boolean).join("&")}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [query]);

  useEffect(() => {
    load();
  }, [load]);

  // The server applies the filters.
  const visible = hub?.models ?? [];
  const total = hub?.total ?? 0;

  const enforced = hub?.guardrails.filter((g) => g.applied !== "on request") ?? [];

  const fileName = (ext: string) => `model-hub${hub?.key ? `-${slug(hub.key.name)}` : ""}-${new Date().toISOString().slice(0, 10)}.${ext}`;

  const exportJson = () => {
    if (!hub) return;
    const doc = { exported_at: new Date().toISOString(), ...(hub.key && { key: hub.key }), guardrails: hub.guardrails, models: visible };
    save(new Blob([JSON.stringify(doc, null, 2)], { type: "application/json" }), fileName("json"));
  };

  const exportXlsx = () => {
    if (!hub) return;
    const modelRows: Cell[][] = [
      ["Model", "Display name", "Vendor", "Description", "Kind", "Mode", "Context (tokens)", "Max output (tokens)", "Input min", "Input max", "Output min", "Output max", "Per 1K searches min", "Per 1K searches max", "Price permutations", "Deployments", "Capabilities", "Tags", "Routes to", "Retires"],
      ...visible.map((m) => {
        const ps = m.deployments.map((d) => d.prices);
        const lo = (f: (p: HubPrices) => number) => (ps.length ? Math.min(...ps.map(f)) : null);
        const hi = (f: (p: HubPrices) => number) => (ps.length ? Math.max(...ps.map(f)) : null);
        return [
          m.name,
          m.metadata.display_name,
          m.metadata.vendor,
          m.metadata.description,
          m.kind,
          m.mode,
          m.max_input_tokens,
          m.max_output_tokens,
          lo((p) => p.input),
          hi((p) => p.input),
          lo((p) => p.output),
          hi((p) => p.output),
          searches(m) ? lo((p) => p.per_k_searches) : null,
          searches(m) ? hi((p) => p.per_k_searches) : null,
          priceOptions(m).length,
          m.deployments.length,
          capabilityNames(m).join(", "),
          m.tags.join(", "),
          m.routes?.join(", "),
          m.retires,
        ];
      }),
    ];
    const priceRows: Cell[][] = [
      ["Model", "Display name", "Vendor", "Deployed as", "Provider", "Upstream model", "Priority", "Weight", "First choice", "Context (tokens)", "Max output (tokens)", "Input", "Output", "Cache read", "Cache write", "Long context above", "Long input", "Long output", "Long cache read", "Long cache write", "Per 1K searches"],
      ...visible.flatMap((m) =>
        m.deployments.map((d): Cell[] => [
          m.name,
          m.metadata.display_name,
          m.metadata.vendor,
          d.model,
          PROVIDER_LABEL[d.provider] ?? d.provider,
          d.upstream_model,
          d.priority,
          d.weight,
          d.first_choice,
          d.max_input_tokens,
          d.max_output_tokens,
          d.prices.input,
          d.prices.output,
          d.prices.cache_read,
          d.prices.cache_write,
          d.prices.long_context_threshold,
          d.prices.long_input,
          d.prices.long_output,
          d.prices.long_cache_read,
          d.prices.long_cache_write,
          searches(m) ? d.prices.per_k_searches : null,
        ]),
      ),
    ];
    const guardrailRows: Cell[][] = [
      ["Guardrail", "Provider", "Applied", "Description"],
      ...hub.guardrails.map((g) => [g.name, GUARDRAIL_LABEL[g.provider] ?? g.provider, g.applied === "on request" ? "on request (x-gateway-guardrails)" : `every request (${g.applied})`, g.description]),
    ];
    const sheets = [
      { name: "Models", rows: modelRows, widths: [28, 24, 16, 48, 8, 10, 14, 16, 10, 10, 10, 10, 12, 12, 10, 12, 36, 20, 28, 12] },
      { name: "Prices", rows: priceRows, widths: [28, 24, 16, 28, 18, 36, 8, 8, 12, 14, 16, 10, 10, 10, 10, 16, 10, 10, 14, 14, 14] },
      { name: "Guardrails", rows: guardrailRows, widths: [28, 20, 32, 48] },
    ];
    if (hub.key) {
      const k = hub.key;
      sheets.push({
        name: "Key",
        rows: [
          ["Field", "Value"],
          ["Name", k.name],
          ["Scope", k.scope],
          ["Project", k.project],
          ["Status", k.status],
          ["Allowed models", k.allowed_models.join(", ") || "all"],
          ["Tags held", k.allowed_tags.join(", ") || "none (no tag restriction)"],
        ],
        widths: [18, 60],
      });
    }
    save(xlsx(sheets), fileName("xlsx"));
  };

  /** The hub as the admin API serves it, with the key and filters picked here. */
  const hubRequest = (): CurlRequest => {
    const query = hubQuery(q.trim());
    return { path: `/admin/api/hub${query ? `?${query}` : ""}` };
  };

  const key = hub?.key;
  return (
    <>
      <div className="page-head">
        <div className="filters">
          <Select
            caption="Key"
            hint="Pick a virtual key to see exactly the models it may use and the guardrails its requests go through."
            value={keyId}
            onChange={setKeyId}
            options={[{ value: "", label: "All models" }, ...keys.map((k) => ({ value: k.id, label: k.name }))]}
          />
          <MultiSelect
            caption="Mode"
            options={[...MODES.filter((m) => knownModes.has(m.value)), ...(knownModes.has("router") ? [{ value: "router", label: "Router" }] : [])]}
            values={modes}
            onChange={setModeFilter}
          />
          {knownVendors.length > 0 && (
            <MultiSelect
              caption="Vendor"
              options={[...knownVendors.map((v) => ({ value: v, label: v })), { value: NO_VENDOR, label: "No vendor" }]}
              values={vendors}
              onChange={setVendorFilter}
            />
          )}
          <MultiSelect
            caption="Capability"
            hint="Only models with all the picked capabilities. Clicking a capability's icon on a model picks it too."
            options={CAPABILITIES.map((c) => ({ value: c.path, label: c.label }))}
            values={capFilter}
            onChange={setCapFilter}
          />
          <TextInput caption="Model" type="search" value={q} onChange={(e) => setQ(e.target.value)} placeholder="name or description" />
          {hub && (
            <span className="muted small">
              {visible.length === total ? `${total} models` : `${visible.length} of ${total} models`}
            </span>
          )}
        </div>
        <div className="page-actions">
          <Segmented
            label="View"
            options={[
              { value: "tiles", label: "Tiles", icon: <TilesIcon /> },
              { value: "list", label: "List", icon: <ListIcon /> },
            ]}
            value={view}
            onChange={pickView}
          />
          <MenuButton
            label="Export"
            className="more-button primary"
            items={[
              { label: "JSON", onClick: exportJson, disabled: !hub },
              { label: "Excel (.xlsx)", onClick: exportXlsx, disabled: !hub },
              { label: "Show curl", title: "The hub with this key and these filters, as a curl command for the admin API.", onClick: () => setCurl(hubRequest()) },
            ]}
          >
            Export
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" aria-hidden="true">
              <path d="m6 9 6 6 6-6" />
            </svg>
          </MenuButton>
        </div>
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {curl && (
        <CurlDialog
          request={curl}
          note={
            <>
              Set <code>GATEWAY_API_KEY</code> to{" "}
              {key ? (
                <>
                  the secret of key <strong>{key.name}</strong>, the master key, an admin key, or a project key of its project.
                </>
              ) : (
                "the master key or an admin key."
              )}
            </>
          }
          onClose={() => setCurl(null)}
        />
      )}
      {key && key.status !== "active" && <Banner tone="warning">{STATUS_TEXT[key.status]}</Banner>}
      {key && (
        <div className="hub-summary">
          <p className="small">
            <strong>{key.name}</strong>
            {key.project && <> · project {key.project}</>}
            {" · "}
            {key.allowed_models.length ? `${key.allowed_models.length} allowed model${key.allowed_models.length === 1 ? "" : "s"}` : "any model"}
            {" · "}
            {key.allowed_tags.length ? <>tags {key.allowed_tags.join(", ")}</> : "no tag restriction"}
          </p>
          {/* Only guardrails the key's requests cannot avoid; those a client may ask for are not shown. */}
          {enforced.length > 0 && (
            <p className="small hub-guardrails">
              <span className="muted">Guardrails on every request</span>
              <span className="tags">
                {enforced.map((g) => (
                  <span key={g.id} className={`tag guardrail${g.applied === "project" ? " inherited" : ""}`} title={`${GUARDRAIL_LABEL[g.provider]}, set on the ${g.applied}${g.description ? `: ${g.description}` : ""}`}>
                    {g.name}
                  </span>
                ))}
              </span>
            </p>
          )}
        </div>
      )}
      {hub && !total ? (
        <div className="empty-card">{key ? "This key may use no model." : "No models yet. Add a deployment on the Deployments tab."}</div>
      ) : tiles ? (
        hub && !visible.length ? (
          <div className="empty-card">No model matches the filter.</div>
        ) : (
          <div className="hub-tiles">
            {visible.map((m) => (
              <ModelTile key={m.name} model={m} keyId={keyId} caps={capFilter} onCap={toggleCap} onEdit={() => setEditing(m)} />
            ))}
          </div>
        )
      ) : (
        <div className="table-wrap">
          <table className="hub-table">
            <colgroup>
              <col className="col-logo" />
              <col />
              <col className="col-stats" />
              <col className="col-tokens" />
              <col className="col-price" />
              <col className="col-caps" />
              <col className="col-actions" />
            </colgroup>
            <thead className="visually-hidden">
              <tr>
                <th aria-label="Vendor logo" />
                <th>Model</th>
                <th>Last 24 hours</th>
                <th className="num">Tokens</th>
                <th className="num">Price</th>
                <th>Capabilities</th>
                <th aria-label="Actions" />
              </tr>
            </thead>
            <tbody>
              {visible.map((m) => {
                const expanded = open === m.name;
                const options = priceOptions(m);
                return (
                  <Fragment key={m.name}>
                    <tr className="clickable" onClick={() => setOpen(expanded ? null : m.name)} aria-expanded={expanded}>
                      <td className="logo middle">
                        <VendorLogo vendor={m.vendor ?? ""} fallback={m.name} />
                      </td>
                      <td>
                        <div className="hub-model-line">
                          <span className="hub-name clip" title={displayName(m)}>
                            {displayName(m)}
                          </span>
                          <Badge tone="neutral">{m.kind === "router" ? "router" : m.mode}</Badge>
                          {m.target && <Badge tone="neutral">alias</Badge>}
                        </div>
                        <div className="muted small clip">
                          <Vendor model={m} />
                          {m.metadata.display_name && <span className="mono" title={m.name}> · {m.name}</span>}
                        </div>
                      </td>
                      <td>
                        <div className="hub-row-stats">
                          <ModelStats model={m} speedSlot />
                        </div>
                      </td>
                      <td className="num">
                        <div className="stat-pairs num">
                          <span title={m.max_input_tokens ? `${m.max_input_tokens.toLocaleString()} input tokens` : "Unknown"}>
                            {m.max_input_tokens ? compact(m.max_input_tokens) : "–"}
                          </span>
                          <span className="muted small">context</span>
                          <span title={m.max_output_tokens ? `${m.max_output_tokens.toLocaleString()} output tokens` : "Unknown"}>
                            {m.max_output_tokens ? compact(m.max_output_tokens) : "–"}
                          </span>
                          <span className="muted small">output</span>
                        </div>
                      </td>
                      <td className="num">
                        <PriceSummary model={m} />
                        {options.length > 1 && <div className="muted small">{options.length} prices</div>}
                      </td>
                      <td className="middle">
                        <CapabilityIcons m={m} picked={capFilter} onPick={toggleCap} />
                      </td>
                      <td className="actions middle" onClick={(e) => e.stopPropagation()}>
                        <div className="hub-tile-actions">
                          <PlayButton model={m.name} keyId={keyId} />
                          <IconButton label={`Edit ${m.name}'s name and description`} title="Edit name and description" onClick={() => setEditing(m)}>
                            <EditIcon />
                          </IconButton>
                          <IconButton label={`${expanded ? "Hide" : "Show"} details of ${m.name}`} title={expanded ? "Hide details" : "Details"} onClick={() => setOpen(expanded ? null : m.name)}>
                            <DetailsIcon />
                          </IconButton>
                        </div>
                      </td>
                    </tr>
                    {expanded && (
                      <tr className="child-row">
                        <td colSpan={7}>
                          <HubDetails model={m} options={options} />
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
              {hub && total > 0 && !visible.length && (
                <tr>
                  <td colSpan={7} className="empty">
                    No model matches the filter.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      )}
      {editing && (
        <MetadataForm
          model={editing}
          vendors={knownVendors}
          onClose={() => setEditing(null)}
          onSaved={async () => {
            setEditing(null);
            await load();
          }}
        />
      )}
    </>
  );
}

/** A price for a tile: "$3", "$0.8–3". */
const dollars = (values: number[]) => (values.length ? `$${range(values)}` : "–");

/** A tile's prices on one line, by the model's mode: input and output per 1M tokens, input alone
 *  for embeddings, per 1K searches for rerank. The cached input price is in the tooltip. */
function TilePrice({ model: m }: { model: HubModel }) {
  const ps = m.deployments.map((d) => d.prices);
  const of = (pick: (p: HubPrices) => number | undefined) => dollars(ps.map(pick).filter((v): v is number => v != null));
  if (m.mode === "rerank") {
    return (
      <div className="hub-tile-price small">
        <span className="mono">{of((p) => p.per_k_searches)}</span> / 1K searches
      </div>
    );
  }
  const cached = m.mode === "embedding" ? "–" : of((p) => p.cache_read);
  return (
    <div className="hub-tile-price small" title={cached !== "–" ? `Cached input ${cached} / 1M` : undefined}>
      <span className="mono">{of((p) => p.input)}</span> in
      {m.mode !== "embedding" && (
        <>
          {" · "}
          <span className="mono">{of((p) => p.output)}</span> out
        </>
      )}{" "}
      / 1M
    </div>
  );
}

/** A success rate: "100%", "99.95%", "87.5%". */
function percent(ok: number, all: number): string {
  const p = (100 * ok) / all;
  return p === 100 ? "100%" : `${+p.toFixed(p >= 99 ? 2 : 1)}%`;
}

/** An hour's share of errors as a tone: none, a few (up to 5%), or more. */
function hourTone(requests: number, errors: number): string {
  if (!requests) return "idle";
  if (!errors) return "good";
  return errors / requests <= 0.05 ? "warning" : "critical";
}

const NO_HOURS: [number, number][] = Array.from({ length: 24 }, () => [0, 0]);

/** The last 24 hours as one bar per hour, coloured by its errors. */
function HourBar({ stats }: { stats?: HubStats }) {
  const hour = 3_600_000;
  const now = Math.floor(Date.now() / hour) * hour;
  const hours = stats?.hours ?? NO_HOURS;
  const time = (i: number) => new Date(now - (hours.length - 1 - i) * hour).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return (
    <span className="status-bar" role="img" aria-label={stats?.requests ? `${stats.errors} of ${stats.requests} requests failed in the last 24 hours` : "No requests in the last 24 hours"}>
      {hours.map(([requests, errors], i) => (
        <span
          key={i}
          className={`status-hour ${hourTone(requests, errors)}`}
          title={requests ? `${time(i)}: ${requests.toLocaleString()} requests, ${errors.toLocaleString()} failed` : `${time(i)}: no requests`}
        />
      ))}
    </span>
  );
}

/** The hour bar with the success rate above it. */
function StatusBar({ stats }: { stats?: HubStats }) {
  return (
    <div className="hub-status">
      <span className="hub-stat-label">Status</span>
      <span className="hub-stat-value">{stats?.requests ? percent(stats.requests - stats.errors, stats.requests) : "–"}</span>
      <HourBar stats={stats} />
    </div>
  );
}

/** Output tokens per second: "239 t/s", "42.5 t/s". */
const speed = (tps?: number) => (tps != null ? `${tps.toFixed(tps >= 100 ? 0 : 1)} t/s` : undefined);

/** How a model ran in the last 24 hours: its status bar, latency and, for chat models, speed. In the list,
 *  `speedSlot` keeps the speed's place for other models so the columns line up from row to row. */
function ModelStats({ model: m, speedSlot }: { model: HubModel; speedSlot?: boolean }) {
  const s = m.stats;
  return (
    <>
      <StatusBar stats={s} />
      <div className="hub-stat" title="Median latency of the successful requests in the last 24 hours">
        <span className="hub-stat-label">Latency</span>
        <span className="hub-stat-value">{ms(s?.p50_latency_ms)}</span>
      </div>
      {m.mode === "chat" ? (
        <div className="hub-stat" title="Output tokens per second while generating, in the last 24 hours">
          <span className="hub-stat-label">Speed</span>
          <span className="hub-stat-value">{speed(s?.tokens_per_second) ?? "–"}</span>
        </div>
      ) : (
        speedSlot && <div className="hub-stat" aria-hidden="true" />
      )}
    </>
  );
}

/** A model as a card: who makes it (its logo, which edits it), what it is for and can do, what it
 *  costs, and how it ran today. */
function ModelTile({
  model: m,
  keyId,
  caps,
  onCap,
  onEdit,
}: {
  model: HubModel;
  /** The hub's key, to try the model as. */
  keyId: string;
  caps: string[];
  onCap: (path: string) => void;
  onEdit: () => void;
}) {
  const vendor = m.vendor ?? "";
  const name = displayName(m);
  const s = m.stats;
  return (
    <article className="card hub-tile">
      <div className="hub-tile-head">
        <button type="button" className="hub-tile-edit" aria-label={`Edit ${name}`} title={vendor ? `${vendor}: edit the model` : "Edit the model"} onClick={onEdit}>
          <VendorLogo vendor={vendor} fallback={m.name} />
        </button>
        <div className="hub-tile-title">
          <div className="hub-name clip" title={name === m.name ? name : `${name} (${m.name})`}>
            {name}
          </div>
          {capabilitiesOf(m).length > 0 && <CapabilityIcons m={m} picked={caps} onPick={onCap} />}
        </div>
        <div className="hub-tile-actions">
          <PlayButton model={m.name} keyId={keyId} />
        </div>
      </div>
      {m.metadata.description && <p className="hub-tile-description">{m.metadata.description}</p>}
      <div className="hub-tile-facts small">
        {/* Most models chat: only the others say what they are. */}
        {(m.kind === "router" || m.mode !== "chat") && <Badge tone="neutral">{m.kind === "router" ? "router" : m.mode}</Badge>}
        {m.target && (
          <span>
            alias of <span className="mono">{m.target}</span>
          </span>
        )}
        {m.max_input_tokens != null && (
          <span
            title={`${m.max_input_tokens.toLocaleString()} input tokens${m.max_output_tokens != null ? `, ${m.max_output_tokens.toLocaleString()} output tokens` : ""}`}
          >
            {compact(m.max_input_tokens)} context
          </span>
        )}
        <TagList tags={m.tags} />
      </div>
      <TilePrice model={m} />
      <div className="hub-tile-foot">
        {s?.requests ? (
          <>
            <HourBar stats={s} />
            <span className="hub-tile-stats mono small" title="Success rate, median latency and output speed in the last 24 hours">
              {[percent(s.requests - s.errors, s.requests), s.p50_latency_ms != null && ms(s.p50_latency_ms), m.mode === "chat" && speed(s.tokens_per_second)]
                .filter(Boolean)
                .join(" · ")}
            </span>
          </>
        ) : (
          <span className="muted small">No requests in 24h</span>
        )}
      </div>
    </article>
  );
}

/** A model's vendor; without one saved, the one its names suggest, marked as such. */
function Vendor({ model }: { model: HubModel }) {
  const vendor = model.vendor;
  if (!vendor) return <span className="muted">–</span>;
  return !model.vendor_suggested ? (
    <span title={vendor}>{vendor}</span>
  ) : (
    <span className="vendor-suggested" title="Suggested from the model's name; edit the model to save it">
      {vendor}
    </span>
  );
}

/** A model's deployments, one row each with what it charges, in the order requests try them. */
function HubDetails({ model: m, options }: { model: HubModel; options: PriceOption[] }) {
  const chat = m.mode === "chat";
  const router = m.kind === "router";
  // Some deployments serve only when the first-choice ones fail.
  const fallback = options.some((o) => !o.firstChoice);
  const facts = [
    m.target && (
      <span key="alias">
        Alias of <span className="mono">{m.target}</span>
      </span>
    ),
    router && m.routes?.length && <span key="routes">Routes to {m.routes.join(", ")}</span>,
    m.retires && <span key="retires">Retires {m.retires}</span>,
  ].filter(Boolean);
  return (
    <div className="hub-details">
      {m.metadata.description && <p className="hub-description">{m.metadata.description}</p>}
      {facts.length > 0 && <p className="small hub-facts">{facts}</p>}
      <h4>{options.length > 1 ? `Deployments: ${options.length} prices` : "Deployments"}</h4>
      {options.length > 1 && (
        <p className="muted small">
          {router
            ? "The router picks one of its models for each request, so the price depends on that model and on which of its deployments serves it."
            : fallback
              ? "Requests go to the first-choice deployments (lowest priority) by weight and fail over to the others, so the price depends on which one serves it."
              : "Requests are spread across the deployments by weight, so the price depends on which one serves it."}
        </p>
      )}
      <table className="detail-table">
        <thead>
          <tr>
            <th>Provider</th>
            <th>Upstream model</th>
            {router && <th>Model</th>}
            <th>Tags</th>
            <th className="num">Priority</th>
            <th className="num">Weight</th>
            {m.mode === "rerank" && <th className="num">Per 1K searches</th>}
            <th className="num">Input</th>
            {m.mode !== "embedding" && m.mode !== "rerank" && <th className="num">Output</th>}
            {chat && <th className="num">Cache read</th>}
            {chat && <th className="num">Cache write</th>}
            {chat && searches(m) && <th className="num">Per 1K web searches</th>}
          </tr>
        </thead>
        <tbody>
          {m.deployments.map((d) => (
            <Fragment key={d.id}>
              <tr>
                <td>{PROVIDER_LABEL[d.provider] ?? d.provider}</td>
                <td className="mono">{d.upstream_model}</td>
                {router && <td className="mono">{d.model}</td>}
                <td>
                  <TagList tags={d.tags} empty="–" />
                </td>
                <td className="num" title={fallback && d.first_choice ? "First choice" : undefined}>
                  {d.priority}
                </td>
                <td className="num">{d.weight}</td>
                {m.mode === "rerank" && <td className="num">{price(d.prices.per_k_searches)}</td>}
                <td className="num">{price(d.prices.input)}</td>
                {m.mode !== "embedding" && m.mode !== "rerank" && <td className="num">{price(d.prices.output)}</td>}
                {chat && <td className="num">{price(d.prices.cache_read)}</td>}
                {chat && <td className="num">{price(d.prices.cache_write)}</td>}
                {chat && searches(m) && <td className="num">{d.prices.per_k_searches ? price(d.prices.per_k_searches) : "–"}</td>}
              </tr>
              {/* Long-context prices in the same columns, on a row of their own after their threshold. */}
              {chat && d.prices.long_context_threshold != null && (
                <tr className="long-prices">
                  <td colSpan={router ? 6 : 5} className="num muted small" title={`Above ${d.prices.long_context_threshold.toLocaleString()} input tokens`}>
                    &gt; {compact(d.prices.long_context_threshold)}
                  </td>
                  <td className="num">{price(d.prices.long_input)}</td>
                  <td className="num">{price(d.prices.long_output)}</td>
                  <td className="num">{price(d.prices.long_cache_read)}</td>
                  <td className="num">{price(d.prices.long_cache_write)}</td>
                  {searches(m) && <td />}
                </tr>
              )}
            </Fragment>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** A model's display name, vendor and description. Other metadata fields are kept as they are. */
function MetadataForm({ model, vendors, onClose, onSaved }: { model: HubModel; vendors: string[]; onClose: () => void; onSaved: () => Promise<void> }) {
  const [name, setName] = useState(model.metadata.display_name ?? "");
  // Without one yet, a guess from the model's names; the form shows it before it is saved.
  const [vendor, setVendor] = useState(model.metadata.vendor ?? model.vendor ?? "");
  const [otherVendor, setOtherVendor] = useState("");
  const [description, setDescription] = useState(model.metadata.description ?? "");
  const vendorOptions = [...new Set([...VENDORS, ...vendors, ...(vendor ? [vendor] : [])])].sort((a, b) => a.localeCompare(b));
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setBusy(true);
    try {
      const { display_name: _n, vendor: _v, description: _d, ...rest } = model.metadata;
      const metadata: ModelMetadata = {
        ...rest,
        ...(name.trim() && { display_name: name.trim() }),
        ...(vendor.trim() && { vendor: vendor.trim() }),
        ...(description.trim() && { description: description.trim() }),
      };
      await api("/model-metadata", { method: "PUT", json: { model: model.name, metadata } });
      await onSaved();
    } catch (e) {
      setError((e as Error).message);
      setBusy(false);
    }
  };

  return (
    <Modal
      title={`Describe ${model.name}`}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy} onClick={submit}>
            Save
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="form">
        <TextInput
          caption="Display name"
          hint="A simpler name for people, such as Claude Opus. Clients still send the model name."
          value={name}
          maxLength={100}
          onChange={(e) => setName(e.target.value)}
          placeholder={model.name}
          autoFocus
        />
        <Select
          caption="Vendor"
          hint={
            model.metadata.vendor || !vendor
              ? "Who makes the model, such as Anthropic or Google; not who hosts it."
              : "Who makes the model, such as Anthropic or Google; not who hosts it. Suggested from the model's name: check it before saving."
          }
          value={vendor}
          onChange={setVendor}
          options={[{ value: "", label: "None" }, ...vendorOptions.map((v) => ({ value: v, label: v }))]}
          footer={
            <div className="select-other">
              <TextInput
                caption="Other"
                value={otherVendor}
                maxLength={100}
                onChange={(e) => setOtherVendor(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && otherVendor.trim()) {
                    e.preventDefault();
                    setVendor(otherVendor.trim());
                    setOtherVendor("");
                  }
                }}
                placeholder="type a vendor, then Enter"
              />
            </div>
          }
        />
        <TextArea
          caption="Description"
          hint="What the model is good at and what it can do: its strengths, inputs it takes, and when to pick it."
          rows={6}
          maxLength={4000}
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          placeholder="e.g. Strongest reasoning and coding; reads images and PDFs. Slower and pricier than Sonnet."
        />
      </div>
    </Modal>
  );
}
