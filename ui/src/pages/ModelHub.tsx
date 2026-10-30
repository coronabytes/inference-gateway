import { Fragment, useCallback, useEffect, useState, type ReactNode } from "react";
import { api, type Hub, type HubDeployment, type HubModel, type HubPrices, type HubStats, type ModelMetadata, type NamedItem } from "../api";
import { Banner, Button, MenuButton, MultiSelect, Segmented, Select, TextArea, TextInput, useDebounced } from "../components/controls";
import { CurlDialog, type CurlRequest } from "../components/CurlButton";
import { TagList } from "../components/Tags";
import { VendorLogo } from "../components/VendorLogo";
import { Badge, BracesIcon, CopyIcon, DocumentIcon, EditIcon, ErrorBanner, GlobeIcon, IconButton, ImageIcon, ListIcon, Modal, ReasoningIcon, TilesIcon } from "../components/ui";
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
function CapabilityIcons({ m }: { m: Capable }) {
  const caps = capabilitiesOf(m);
  if (!caps.length) return <span className="muted">–</span>;
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

/** The headline price of a model: what its deployments charge, as a range where they differ. */
function priceSummary(m: HubModel): { value: string; unit: string } {
  const ps = m.deployments.map((d) => d.prices);
  if (m.mode === "rerank") return { value: range(ps.map((p) => p.per_k_searches)), unit: "$ per 1K searches" };
  if (m.mode === "embedding") return { value: range(ps.map((p) => p.input)), unit: "$ in per 1M" };
  return { value: `${range(ps.map((p) => p.input))} · ${range(ps.map((p) => p.output))}`, unit: "$ in · out per 1M" };
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
  const [open, setOpen] = useState<string | null>(null);
  const [editing, setEditing] = useState<HubModel | null>(null);
  const [curl, setCurl] = useState<CurlRequest | null>(null);
  const [view, setView] = useState<View>(initialView);
  // The model whose details are open in a dialog, in the tile view.
  const [details, setDetails] = useState<HubModel | null>(null);

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
    if (term) params.set("q", term);
    return params.toString().replace(/%2C/g, ",");
  };
  const query = hubQuery(search);
  // The tiles show each model's last 24 hours from the request log; the list does not.
  const tiles = view === "tiles";

  const load = useCallback(async () => {
    try {
      const q = [query, tiles && "stats=true"].filter(Boolean).join("&");
      setHub(await api<Hub>(`/hub${q ? `?${q}` : ""}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [query, tiles]);

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
              <ModelTile key={m.name} model={m} onEdit={() => setEditing(m)} onDetails={() => setDetails(m)} />
            ))}
          </div>
        )
      ) : (
        <div className="table-wrap">
          <table className="hub-table">
            <colgroup>
              <col />
              <col className="col-vendor" />
              <col className="col-mode" />
              <col className="col-tokens" span={2} />
              <col className="col-price" />
              <col className="col-caps" />
              <col className="col-actions" />
            </colgroup>
            <thead>
              <tr>
                <th>Model</th>
                <th>Vendor</th>
                <th>Mode</th>
                <th className="num">Context</th>
                <th className="num">Max output</th>
                <th className="num">Price</th>
                <th>Capabilities</th>
                <th aria-label="Actions" />
              </tr>
            </thead>
            <tbody>
              {visible.map((m) => {
                const expanded = open === m.name;
                const p = priceSummary(m);
                const options = priceOptions(m);
                return (
                  <Fragment key={m.name}>
                    <tr className="clickable" onClick={() => setOpen(expanded ? null : m.name)} aria-expanded={expanded}>
                      <td>
                        <div className="hub-name clip" title={displayName(m)}>
                          {displayName(m)}
                        </div>
                        {m.metadata.display_name && (
                          <div className="mono muted small clip" title={m.name}>
                            {m.name}
                          </div>
                        )}
                      </td>
                      <td className="clip">
                        <Vendor model={m} />
                      </td>
                      <td>
                        <Badge tone="neutral">{m.kind === "router" ? "router" : m.mode}</Badge>
                        {m.target && (
                          <>
                            {" "}
                            <Badge tone="neutral">alias</Badge>
                          </>
                        )}
                      </td>
                      <td className="num" title={m.max_input_tokens ? `${m.max_input_tokens.toLocaleString()} input tokens` : "Unknown"}>
                        {m.max_input_tokens ? compact(m.max_input_tokens) : "–"}
                      </td>
                      <td className="num" title={m.max_output_tokens ? `${m.max_output_tokens.toLocaleString()} output tokens` : "Unknown"}>
                        {m.max_output_tokens ? compact(m.max_output_tokens) : "–"}
                      </td>
                      <td className="num">
                        <div>{p.value}</div>
                        <div className="muted small">{options.length > 1 ? `${options.length} prices` : p.unit}</div>
                      </td>
                      <td>
                        <CapabilityIcons m={m} />
                      </td>
                      <td className="actions" onClick={(e) => e.stopPropagation()}>
                        <IconButton label={`Edit ${m.name}'s name and description`} title="Edit name and description" onClick={() => setEditing(m)}>
                          <EditIcon />
                        </IconButton>
                      </td>
                    </tr>
                    {expanded && (
                      <tr className="child-row">
                        <td colSpan={8}>
                          <HubDetails model={m} options={options} />
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
              {hub && total > 0 && !visible.length && (
                <tr>
                  <td colSpan={8} className="empty">
                    No model matches the filter.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      )}
      {details && (
        <Modal title={displayName(details)} size="wide" onClose={() => setDetails(null)} footer={<Button onClick={() => setDetails(null)}>Close</Button>}>
          <HubDetails model={details} options={priceOptions(details)} />
        </Modal>
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

/** The prices a tile shows, by the model's mode: per 1M tokens, or per 1K searches for rerank. */
function tilePrices(m: HubModel): { label: string; value: string; unit: string }[] {
  const ps = m.deployments.map((d) => d.prices);
  if (m.mode === "rerank") return [{ label: "Searches", value: dollars(ps.map((p) => p.per_k_searches)), unit: "1K" }];
  if (m.mode === "embedding") return [{ label: "Input", value: dollars(ps.map((p) => p.input)), unit: "1M" }];
  return [
    { label: "Input", value: dollars(ps.map((p) => p.input)), unit: "1M" },
    { label: "Output", value: dollars(ps.map((p) => p.output)), unit: "1M" },
    { label: "Cached", value: dollars(ps.map((p) => p.cache_read)), unit: "1M" },
  ];
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

/** The last 24 hours as one bar per hour, coloured by its errors, with the success rate. */
function StatusBar({ stats }: { stats?: HubStats }) {
  const hour = 3_600_000;
  const now = Math.floor(Date.now() / hour) * hour;
  const hours = stats?.hours ?? NO_HOURS;
  const time = (i: number) => new Date(now - (hours.length - 1 - i) * hour).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return (
    <div className="hub-status">
      <span className="hub-stat-label">Status</span>
      <span className="hub-stat-value">{stats?.requests ? percent(stats.requests - stats.errors, stats.requests) : "–"}</span>
      <span className="status-bar" role="img" aria-label={stats?.requests ? `${stats.errors} of ${stats.requests} requests failed in the last 24 hours` : "No requests in the last 24 hours"}>
        {hours.map(([requests, errors], i) => (
          <span
            key={i}
            className={`status-hour ${hourTone(requests, errors)}`}
            title={requests ? `${time(i)}: ${requests.toLocaleString()} requests, ${errors.toLocaleString()} failed` : `${time(i)}: no requests`}
          />
        ))}
      </span>
    </div>
  );
}

/** A model as a card: who makes it, what it is for and can do, what it costs, and how it ran today. */
function ModelTile({ model: m, onEdit, onDetails }: { model: HubModel; onEdit: () => void; onDetails: () => void }) {
  const [copied, setCopied] = useState(false);
  const vendor = m.vendor ?? "";
  const s = m.stats;
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(m.name);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // The browser refused; the name is on the card to copy by hand.
    }
  };
  return (
    <article className="card hub-tile">
      <div className="hub-tile-head">
        <VendorLogo vendor={vendor} fallback={m.name} />
        <div className="hub-tile-title">
          <div className="hub-name clip" title={displayName(m)}>
            {displayName(m)}
          </div>
          <div className="muted small clip">
            <Vendor model={m} />
            {m.metadata.display_name && <span className="mono"> · {m.name}</span>}
          </div>
        </div>
        <div className="hub-tile-actions">
          <IconButton label={`Edit ${m.name}'s name and description`} title="Edit name and description" onClick={onEdit}>
            <EditIcon />
          </IconButton>
          <IconButton label={copied ? "Copied" : `Copy the model name ${m.name}`} title={copied ? "Copied" : "Copy the model name"} onClick={copy}>
            <CopyIcon />
          </IconButton>
        </div>
      </div>
      {m.metadata.description && <p className="hub-tile-description">{m.metadata.description}</p>}
      <div className="hub-tile-facts small">
        <Badge tone="neutral">{m.kind === "router" ? "router" : m.mode}</Badge>
        {m.target && <Badge tone="neutral">alias</Badge>}
        {m.max_input_tokens != null && <span title={`${m.max_input_tokens.toLocaleString()} input tokens`}>{compact(m.max_input_tokens)} context</span>}
        {m.max_output_tokens != null && <span title={`${m.max_output_tokens.toLocaleString()} output tokens`}>{compact(m.max_output_tokens)} out</span>}
        <CapabilityIcons m={m} />
      </div>
      {m.tags.length > 0 && <TagList tags={m.tags} />}
      <dl className="hub-tile-prices">
        {tilePrices(m).map((p) => (
          <div key={p.label}>
            <dt className="muted small">{p.label}</dt>
            <dd>
              <span className="mono">{p.value}</span> <span className="muted small">/ {p.unit}</span>
            </dd>
          </div>
        ))}
      </dl>
      <div className="hub-tile-foot">
        <StatusBar stats={s} />
        <div className="hub-stat" title="Median latency of the successful requests in the last 24 hours">
          <span className="hub-stat-label">Latency</span>
          <span className="hub-stat-value">{ms(s?.p50_latency_ms)}</span>
        </div>
        {m.mode === "chat" && (
          <div className="hub-stat" title="Output tokens per second while generating, in the last 24 hours">
            <span className="hub-stat-label">Speed</span>
            <span className="hub-stat-value">{s?.tokens_per_second != null ? `${s.tokens_per_second.toFixed(s.tokens_per_second >= 100 ? 0 : 1)} t/s` : "–"}</span>
          </div>
        )}
        <Button variant="ghost" onClick={onDetails}>
          Details ›
        </Button>
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

/** A model's prices, one row per price the deployments that may serve it charge, and its deployments. */
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
    m.tags.length > 0 && <TagList key="tags" tags={m.tags} />,
    router && m.routes?.length && <span key="routes">Routes to {m.routes.join(", ")}</span>,
    m.retires && <span key="retires">Retires {m.retires}</span>,
  ].filter(Boolean);
  return (
    <div className="hub-details">
      {m.metadata.description && <p className="hub-description">{m.metadata.description}</p>}
      {facts.length > 0 && <p className="small hub-facts">{facts}</p>}
      <h4>{options.length > 1 ? `Prices: ${options.length} permutations` : "Price"}</h4>
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
            {m.mode === "rerank" && <th className="num">Per 1K searches</th>}
            <th className="num">Input</th>
            {m.mode !== "embedding" && m.mode !== "rerank" && <th className="num">Output</th>}
            {chat && <th className="num">Cache read</th>}
            {chat && <th className="num">Cache write</th>}
            {chat && <th>Long context</th>}
            {chat && searches(m) && <th className="num">Per 1K web searches</th>}
            <th>Served by</th>
          </tr>
        </thead>
        <tbody>
          {options.map((o) => (
            <tr key={o.deployments[0].id}>
              {m.mode === "rerank" && <td className="num">{price(o.prices.per_k_searches)}</td>}
              <td className="num">{price(o.prices.input)}</td>
              {m.mode !== "embedding" && m.mode !== "rerank" && <td className="num">{price(o.prices.output)}</td>}
              {chat && <td className="num">{price(o.prices.cache_read)}</td>}
              {chat && <td className="num">{price(o.prices.cache_write)}</td>}
              {chat && (
                <td>
                  {o.prices.long_context_threshold != null
                    ? `above ${compact(o.prices.long_context_threshold)}: ${price(o.prices.long_input)} · ${price(o.prices.long_output)}`
                    : "–"}
                </td>
              )}
              {chat && searches(m) && <td className="num">{o.prices.per_k_searches ? price(o.prices.per_k_searches) : "–"}</td>}
              <td>
                {o.deployments.map((d) => (
                  <div key={d.id} className="small">
                    {PROVIDER_LABEL[d.provider] ?? d.provider} <span className="mono">{d.upstream_model}</span>
                    {router && <span className="muted"> as {d.model}</span>}
                  </div>
                ))}
                {o.firstChoice && fallback && <Badge tone="good">first choice</Badge>}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="muted small">
        {m.mode === "rerank" ? "USD per 1,000 searches; tokens per 1M." : searches(m) ? "USD per 1M tokens; web searches per 1,000." : "USD per 1M tokens."}
      </p>
      <h4>Deployments</h4>
      <table className="detail-table">
        <thead>
          <tr>
            <th>Provider</th>
            <th>Upstream model</th>
            {router && <th>Model</th>}
            <th className="num">Priority</th>
            <th className="num">Weight</th>
            <th className="num">Context</th>
            <th className="num">Max output</th>
            <th>Capabilities</th>
          </tr>
        </thead>
        <tbody>
          {m.deployments.map((d) => (
            <tr key={d.id}>
              <td>{PROVIDER_LABEL[d.provider] ?? d.provider}</td>
              <td className="mono">{d.upstream_model}</td>
              {router && <td className="mono">{d.model}</td>}
              <td className="num">{d.priority}</td>
              <td className="num">{d.weight}</td>
              <td className="num">{d.max_input_tokens ? compact(d.max_input_tokens) : "–"}</td>
              <td className="num">{d.max_output_tokens ? compact(d.max_output_tokens) : "–"}</td>
              <td>
                <CapabilityIcons m={d} />
              </td>
            </tr>
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
