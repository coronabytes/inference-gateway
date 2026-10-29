import { Fragment, useEffect, useMemo, useState } from "react";
import { api, type Hub, type HubDeployment, type HubModel, type HubPrices, type ModelMetadata, type VirtualKey } from "../api";
import { Button, MenuButton, MultiSelect, Select, TextArea, TextInput } from "../components/controls";
import { CurlDialog, type CurlRequest } from "../components/CurlButton";
import { TagList } from "../components/Tags";
import { Badge, EditIcon, ErrorBanner, IconButton, Modal } from "../components/ui";
import { compact } from "../format";
import { PROVIDER_LABEL } from "../providers";
import { xlsx, type Cell } from "../xlsx";
import { GUARDRAIL_LABEL } from "./Guardrails";
import { MODES } from "./Models";

/** Capabilities the hub names, by path in a model's `capabilities`. */
const CAPABILITIES: [string, string][] = [
  ["image_input", "Images"],
  ["pdf_input", "PDF"],
  ["thinking.supported", "Reasoning"],
  ["structured_outputs", "Structured output"],
  ["server_tools.web_search", "Web search"],
];

const capabilityNames = (m: { capabilities: Record<string, boolean | null> | null }) => CAPABILITIES.filter(([path]) => m.capabilities?.[path] === true).map(([, label]) => label);

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

/** The mode filter's value for a model: its mode, or "router" for an auto router. */
const modeOf = (m: HubModel): string => (m.kind === "router" ? "router" : m.mode);

/** Vendors offered in the editor; others can be typed. */
const VENDORS = ["Alibaba", "Amazon", "Anthropic", "Cohere", "DeepSeek", "Google", "Meta", "Microsoft", "Mistral AI", "Moonshot AI", "OpenAI", "TypeSafe", "xAI", "Zhipu AI"];

/** Who likely makes a model, from its name and its upstream models: claude → Anthropic. */
const VENDOR_PATTERNS: [RegExp, string][] = [
  [/claude|anthropic/, "Anthropic"],
  [/\bgpt|\bo[1-9]\b|openai|text-embedding-|davinci|whisper/, "OpenAI"],
  [/gemini|gemma|google|palm/, "Google"],
  [/mistral|mixtral|codestral|pixtral|ministral|magistral/, "Mistral AI"],
  [/llama|\bmeta\b/, "Meta"],
  [/deepseek/, "DeepSeek"],
  [/qwen|alibaba/, "Alibaba"],
  [/\bglm|zhipu|z-ai/, "Zhipu AI"],
  [/kimi|moonshot/, "Moonshot AI"],
  [/cohere|command-r|\bembed-(english|multilingual)/, "Cohere"],
  [/titan|amazon\.nova|\bnova-/, "Amazon"],
  [/microsoft|\bphi-|decision-1/, "Microsoft"],
  [/grok|x-ai|xai/, "xAI"],
  [/\bjev/, "TypeSafe"],
];

function guessVendor(m: HubModel): string {
  const names = [m.name, ...m.deployments.map((d) => d.upstream_model)].join(" ").toLowerCase();
  return VENDOR_PATTERNS.find(([re]) => re.test(names))?.[1] ?? "";
}

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

/** A file name's part for a key: "Team A (prod)" → "team-a-prod". */
const slug = (s: string) => s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") || "key";

/**
 * The model hub: every model the gateway serves, or with a key picked those that key may use, with
 * token limits, capabilities, the prices of the deployments that may serve each one, and the
 * guardrails the key's requests go through. Each model's display name and description are edited
 * here.
 */
export function ModelHub() {
  const [keys, setKeys] = useState<VirtualKey[]>([]);
  const [keyId, setKeyId] = useState("");
  const [hub, setHub] = useState<Hub | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [modeFilter, setModeFilter] = useState<string[]>([]);
  const [q, setQ] = useState("");
  const [vendorFilter, setVendorFilter] = useState<string[]>([]);
  const [open, setOpen] = useState<string | null>(null);
  const [editing, setEditing] = useState<HubModel | null>(null);
  const [curl, setCurl] = useState<CurlRequest | null>(null);

  useEffect(() => {
    api<VirtualKey[]>("/keys")
      .then((k) => setKeys([...k].sort((a, b) => a.name.localeCompare(b.name))))
      .catch((e) => setError((e as Error).message));
  }, []);

  const load = async () => {
    try {
      setHub(await api<Hub>(`/hub${keyId ? `?key=${encodeURIComponent(keyId)}` : ""}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  useEffect(() => {
    load();
  }, [keyId]);

  const models = hub?.models ?? [];
  const knownModes = useMemo(() => new Set(models.map(modeOf)), [models]);
  const knownVendors = useMemo(() => [...new Set(models.flatMap((m) => (m.metadata.vendor ? [m.metadata.vendor] : [])))].sort((a, b) => a.localeCompare(b)), [models]);
  // A picked mode or vendor no models have any more drops out.
  const modes = modeFilter.filter((m) => knownModes.has(m));
  const vendors = vendorFilter.filter((v) => v === NO_VENDOR || knownVendors.includes(v));
  const visible = useMemo(() => {
    const term = q.trim().toLowerCase();
    return models.filter(
      (m) =>
        (!modes.length || modes.includes(modeOf(m))) &&
        (!term ||
          [m.name, m.metadata.display_name, m.metadata.vendor, m.metadata.description, ...m.deployments.map((d) => d.upstream_model)].some((s) =>
            s?.toLowerCase().includes(term),
          )) &&
        (!vendors.length || vendors.includes(m.metadata.vendor ?? NO_VENDOR)),
    );
  }, [models, modes.join(), q, vendors.join()]);

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
          m.mode === "rerank" ? lo((p) => p.per_k_searches) : null,
          m.mode === "rerank" ? hi((p) => p.per_k_searches) : null,
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
          m.mode === "rerank" ? d.prices.per_k_searches : null,
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
    const params = new URLSearchParams();
    if (keyId) params.set("key", keyId);
    if (modes.length) params.set("mode", modes.join(","));
    if (vendors.length) params.set("vendor", vendors.map((v) => (v === NO_VENDOR ? "-" : v)).join(","));
    if (q.trim()) params.set("q", q.trim());
    const query = params.toString().replace(/%2C/g, ",");
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
              {visible.length === models.length ? `${models.length} models` : `${visible.length} of ${models.length} models`}
            </span>
          )}
        </div>
        <div className="page-actions">
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
      <ErrorBanner error={error} />
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
      {key && key.status !== "active" && <div className="banner warn">{STATUS_TEXT[key.status]}</div>}
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
      {hub && !models.length ? (
        <div className="empty-card">{key ? "This key may use no model." : "No models yet. Add a deployment on the Deployments tab."}</div>
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
                        <span className="tags">
                          {capabilityNames(m).map((c) => (
                            <span key={c} className="tag">
                              {c}
                            </span>
                          ))}
                        </span>
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
              {hub && models.length > 0 && !visible.length && (
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

/** A model's vendor; without one saved, the guess from its names, marked as such. */
function Vendor({ model }: { model: HubModel }) {
  const saved = model.metadata.vendor;
  const vendor = saved || guessVendor(model);
  if (!vendor) return <span className="muted">–</span>;
  return saved ? (
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
      <p className="muted small">{m.mode === "rerank" ? "USD per 1,000 searches; tokens per 1M." : "USD per 1M tokens."}</p>
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
              <td>{capabilityNames(d).join(", ") || "–"}</td>
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
  const [vendor, setVendor] = useState(model.metadata.vendor ?? guessVendor(model));
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
      <ErrorBanner error={error} />
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
