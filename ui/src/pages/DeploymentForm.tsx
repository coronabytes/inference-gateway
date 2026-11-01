import { useEffect, useState } from "react";
import {
  api,
  type ApiStyle,
  type BedrockModel,
  type BedrockModels,
  type Credential,
  type Deployment,
  type DeploymentInput,
  type FoundryDeployment,
  type FoundryDeployments,
  type Mode,
  type NamedItem,
  type PriceEntry,
  type Provider,
} from "../api";
import { CatalogPicker } from "../components/CatalogPicker";
import { Banner, Button, MultiSelect, Segmented, Select, Switch, TextInput } from "../components/controls";
import { TagEditor } from "../components/Tags";
import { ErrorBanner, Modal, Section, optInt, optNum } from "../components/ui";
import { compact } from "../format";
import { catalogScope, deploymentPrices, suggestModelName, upstreamFromKey } from "../pricing";
import { PROVIDER_LABEL } from "../providers";
import { connectionSummary, usesEntra } from "./Credentials";

/**
 * A protocol choice. Bedrock offers Claude's Messages API two ways, both `anthropic` upstream:
 * `invoke` (InvokeModel) and `anthropic` (bedrock-mantle, the default before the choice existed).
 */
type StyleChoice = ApiStyle | "invoke";

/** Chat protocols per provider; embedding, decision and rerank deployments have one each. The note is
 *  what the protocol is for there; it may be cut off in the button. */
const STYLES: Record<Provider, { value: StyleChoice; label: string; note?: string }[]> = {
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
    { value: "invoke", label: "Anthropic Messages", note: "Claude via InvokeModel" },
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

/** Providers that serve each mode. */
const MODE_PROVIDERS: Record<Mode, Provider[]> = {
  chat: ["openai", "azure", "bedrock", "anthropic", "vertex", "gemini", "mistral"],
  embedding: ["openai", "azure", "bedrock", "vertex", "gemini", "mistral"],
  decision: ["typesafe", "azure"],
  rerank: ["azure", "bedrock", "openai"],
};

/** List price of TypeSafe Jev and Microsoft-Decision-1: $0.042 per 1M input tokens, output free. */
const DECISION_INPUT_PRICE = "0.042";

/** List price of Amazon Titan Text Embeddings V2 on Bedrock: $0.02 per 1M input tokens. */
const TITAN_INPUT_PRICE = "0.02";
const TITAN_MODEL = "amazon.titan-embed-text-v2:0";

/** What a Foundry deployment runs and what its quota allows: "gpt-4o 2024-11-20 · GlobalStandard · 2.7K RPM, 450K TPM". */
function foundryNote(d: FoundryDeployment): string {
  const limits = [d.rpm != null && `${compact(d.rpm)} RPM`, d.tpm != null && `${compact(d.tpm)} TPM`].filter(Boolean).join(", ");
  const state = d.state && d.state !== "Succeeded" ? d.state.toLowerCase() : "";
  const sku = d.sku && d.data_zone ? `${d.sku} (${d.data_zone.toUpperCase()})` : d.sku;
  return [[d.model, d.model_version].filter(Boolean).join(" "), sku, limits, state].filter(Boolean).join(" · ");
}

const BEDROCK_KIND: Record<BedrockModel["kind"], string> = { on_demand: "on-demand", cross_region: "cross-region profile", global: "global profile" };

/** How a Bedrock model is called and what its account quota allows: "cross-region profile · 200 RPM, 2M TPM". */
function bedrockNote(b: BedrockModel): string {
  const limits = [b.rpm != null && `${compact(b.rpm)} RPM`, b.tpm != null && `${compact(b.tpm)} TPM`].filter(Boolean).join(", ");
  return [BEDROCK_KIND[b.kind], limits, b.legacy && "legacy"].filter(Boolean).join(" · ");
}

/** The dialog that adds, edits or duplicates a deployment. */
export function DeploymentForm({
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
  projects: NamedItem[];
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
  const [style, setStyle] = useState<StyleChoice>(
    from?.config.bedrock_api === "invoke" ? "invoke" : (from?.config.api_style ?? STYLES[from?.provider ?? "openai"][0]?.value ?? "chat"),
  );
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
  // Bedrock has no web search to bill (see `Caps::of` in api/models.rs).
  const webSearch = chat && provider !== "bedrock";
  const anthropicStyle = chat && (style === "anthropic" || style === "invoke" || (provider === "bedrock" && style === "converse"));
  const available = credentials.filter((c) => c.provider === provider);
  const selected = available.find((c) => c.id === credentialId);
  // Azure credentials with Entra ID can list their resource's deployments.
  const listable = selected && usesEntra(selected) ? selected.id : null;
  const [foundry, setFoundry] = useState<FoundryDeployments | null>(null);
  const [foundryError, setFoundryError] = useState<string | null>(null);
  useEffect(() => {
    setFoundry(null);
    setFoundryError(null);
    if (!listable) return;
    let live = true;
    api<FoundryDeployments>(`/credentials/${listable}/foundry-deployments`)
      .then((r) => live && setFoundry(r))
      .catch((e) => live && setFoundryError((e as Error).message));
    return () => {
      live = false;
    };
  }, [listable]);
  // Bedrock credentials with AWS credentials (not a Bedrock API key) can list models and quotas.
  const bedrockListable = provider === "bedrock" && selected && !selected.secrets_set.api_key ? selected.id : null;
  const [bedrock, setBedrock] = useState<BedrockModels | null>(null);
  const [bedrockError, setBedrockError] = useState<string | null>(null);
  useEffect(() => {
    setBedrock(null);
    setBedrockError(null);
    if (!bedrockListable) return;
    let live = true;
    api<BedrockModels>(`/credentials/${bedrockListable}/bedrock-models`)
      .then((r) => live && setBedrock(r))
      .catch((e) => live && setBedrockError((e as Error).message));
    return () => {
      live = false;
    };
  }, [bedrockListable]);

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
    webSearch && optNum(perK) && `${price(perK)} per 1K web searches`,
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
  const applyCatalog = (entry: PriceEntry, auto: boolean, m: Mode = mode) => {
    // The same conversion the price check uses, so a filled-in deployment always matches it.
    const p = deploymentPrices(entry.data, m);
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

  /**
   * Fill the form from a deployment on the Foundry resource: its name as the upstream model, its
   * mode and protocol, the catalog entry of its model (with prices) and its quota as the limits.
   * The catalog is looked up first, so everything changes at once and the upstream model's own
   * catalog lookup finds the base model already picked.
   */
  const pickFoundry = async (name: string) => {
    const f = foundry?.deployments.find((d) => d.name === name);
    if (!f) return;
    const m = initial ? mode : f.mode;
    // Most specific first: the data zone's price (azure/eu/…, azure/us/…), the model's version
    // (prices change between versions), then the model without either.
    const models = [f.model_version && `${f.model}-${f.model_version}`, f.model].filter((x): x is string => !!x);
    const zoned = f.data_zone ? models.map((x) => `${f.data_zone}/${x}`) : [];
    let entry: PriceEntry | null = null;
    for (const model of [...zoned, ...models]) {
      try {
        entry = (await api<{ exact: PriceEntry | null }>(`/prices/match?provider=azure&model=${encodeURIComponent(model)}`)).exact;
      } catch {
        /* the catalog is optional */
      }
      if (entry) break;
    }
    const zoneMissing = !!f.data_zone && !!entry && !entry.key.includes(`/${f.data_zone}/`);
    if (m !== mode) {
      setMode(m);
      prefill(m, provider);
    }
    if (m === "chat" && f.api_style) setStyle(f.api_style);
    setUpstream(f.name);
    if (nameAuto) setModelName(suggestModelName(f.model));
    setRpm(f.rpm != null ? String(f.rpm) : "");
    setTpm(f.tpm != null ? String(f.tpm) : "");
    if (entry) {
      applyCatalog(entry, false, m);
      if (zoneMissing)
        setPricingNote(
          `The price catalog has no ${f.data_zone!.toUpperCase()} data zone entry for ${f.model}; prices are filled from ${entry.key}. Data zone prices are usually higher, so check them before saving.`,
        );
    } else setPricingNote(`The price catalog has no entry for ${f.model}; enter its prices yourself.`);
  };

  /**
   * Fill the form from a Bedrock model or inference profile: its ID as the upstream model, its
   * mode, the catalog entry with prices, and the account's quota as the limits when it could be
   * read (otherwise the limits stay as they are, to be entered by hand).
   */
  const pickBedrock = async (id: string) => {
    const b = bedrock?.models.find((x) => x.id === id);
    if (!b) return;
    const m = initial ? mode : b.mode;
    let entry: PriceEntry | null = null;
    try {
      entry = (await api<{ exact: PriceEntry | null }>(`/prices/match?provider=bedrock&model=${encodeURIComponent(b.id)}`)).exact;
    } catch {
      /* the catalog is optional */
    }
    if (m !== mode) {
      setMode(m);
      prefill(m, provider);
    }
    setUpstream(b.id);
    if (nameAuto) setModelName(suggestModelName(b.id));
    if (b.rpm != null) setRpm(String(b.rpm));
    if (b.tpm != null) setTpm(String(b.tpm));
    if (entry) applyCatalog(entry, false, m);
    else setPricingNote(`The price catalog has no entry for ${b.id}; enter its prices yourself.`);
  };
  // Bedrock quotas belong to the account, not the deployment: others on this credential with the
  // same model share them. The server lists the credential's deployments, whatever the page shows.
  const [onCredential, setOnCredential] = useState<Deployment[]>([]);
  useEffect(() => {
    setOnCredential([]);
    if (provider !== "bedrock" || !credentialId) return;
    let live = true;
    api<Deployment[]>(`/deployments?credentials=${credentialId}`)
      .then((r) => live && setOnCredential(r))
      .catch(() => {
        /* only for the note below */
      });
    return () => {
      live = false;
    };
  }, [provider, credentialId]);
  const sharing = onCredential.filter((d) => d.id !== initial?.id && d.upstream_model === upstream.trim());

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
          ...(chat && { api_style: style === "invoke" ? "anthropic" : style }),
          ...(chat && provider === "bedrock" && style === "invoke" && { bedrock_api: "invoke" as const }),
          ...(chat && provider === "bedrock" && style === "anthropic" && { bedrock_api: "mantle" as const }),
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
        cost_per_k_searches: mode === "rerank" || webSearch ? optNum(perK) : null,
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
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
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
          <Banner tone="warning">
            No {PROVIDER_LABEL[provider]} credential yet. <a href="#/models/credentials">Create one</a> first.
          </Banner>
        )}
        <fieldset>
          <legend>Model</legend>
          {listable && (
            <div className="field">
              <Select
                caption="Foundry deployment"
                hint={
                  <>
                    The deployments on {foundry ? <span className="mono">{foundry.resource.split("/").pop()}</span> : "the credential's resource"}, read
                    with its Entra ID app. Picking one fills in the upstream model, protocol, base model with its prices, and the RPM and TPM
                    limits of its quota.
                  </>
                }
                value={foundry?.deployments.some((d) => d.name === upstream) ? upstream : ""}
                onChange={pickFoundry}
                empty={foundryError ? "Not available" : foundry ? "No deployments" : "Loading…"}
                options={(foundry?.deployments ?? []).map((d) => ({
                  value: d.name,
                  label: d.name,
                  group: MODES.find((x) => x.value === d.mode)!.label,
                  note: foundryNote(d),
                  disabled: !!initial && d.mode !== mode,
                }))}
              />
              {foundryError && <p className="muted small">Could not list the deployments: {foundryError}</p>}
            </div>
          )}
          {bedrockListable && (
            <div className="field">
              <Select
                caption="Bedrock model"
                hint={
                  <ul className="hint-list">
                    <li>
                      The models and inference profiles the credential can call{bedrock ? ` in ${bedrock.region}` : ""}. Picking one fills in the
                      upstream model, base model with its prices, and the RPM and TPM limits of the account&apos;s quota for it.
                    </li>
                    <li>
                      <strong>Quotas belong to the AWS account and region</strong>, not to a deployment: everything using the account shares them.
                      Lower the limits when other deployments or applications use the same model.
                    </li>
                    <li>
                      Bedrock may count a model&apos;s output tokens several times against its TPM quota (its token burndown rate), so the
                      gateway&apos;s TPM limit can let more through than the quota allows. Check the model&apos;s rate in the AWS documentation.
                    </li>
                    <li>
                      <strong>Permissions:</strong> listing needs <span className="mono">bedrock:ListFoundationModels</span> and{" "}
                      <span className="mono">bedrock:ListInferenceProfiles</span>, the limits{" "}
                      <span className="mono">servicequotas:ListServiceQuotas</span>. Without them, type the values in.
                    </li>
                  </ul>
                }
                value={bedrock?.models.some((b) => b.id === upstream) ? upstream : ""}
                onChange={pickBedrock}
                empty={bedrockError ? "Not available" : bedrock ? "No models" : "Loading…"}
                options={(bedrock?.models ?? []).map((b) => ({
                  value: b.id,
                  label: b.name,
                  group: MODES.find((x) => x.value === b.mode)!.label,
                  note: bedrockNote(b),
                  disabled: !!initial && b.mode !== mode,
                }))}
              />
              {bedrockError && <p className="muted small">Could not list the models: {bedrockError}</p>}
              {bedrock?.models_error && <p className="muted small">{bedrock.models_error} Enter the upstream model yourself.</p>}
              {bedrock?.quotas_error && <p className="muted small">{bedrock.quotas_error} Enter the RPM and TPM limits yourself.</p>}
              {sharing.length > 0 && (
                <p className="muted small">
                  {sharing.map((d) => d.model_name).join(", ")} {sharing.length === 1 ? "uses" : "use"} the same model on this credential and
                  {sharing.length === 1 ? " shares" : " share"} its quota. Split the limits between them.
                </p>
              )}
            </div>
          )}
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
              <Select
                caption="Protocol"
                hint={
                  provider === "bedrock" ? (
                    <ul className="hint-list">
                      <li>
                        <strong>Converse</strong> works with every Bedrock chat model.
                      </li>
                      <li>
                        <strong>Anthropic Messages via InvokeModel</strong> sends Claude its own API through Bedrock Runtime, in any region or
                        inference profile that serves the model, with token counting.
                      </li>
                      <li>
                        <strong>via bedrock-mantle</strong> calls Bedrock&apos;s Anthropic-compatible endpoint, where it is available.
                      </li>
                    </ul>
                  ) : undefined
                }
                value={style}
                onChange={(s) => setStyle(s as StyleChoice)}
                options={STYLES[provider]}
              />
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
            <div className={webSearch ? "row wide-first" : "row"}>
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
              {webSearch && (
                <TextInput
                  caption="Per 1K web searches"
                  hint="Each web search the model runs, as the provider bills it. Empty when the model has none."
                  inputMode="decimal"
                  value={perK}
                  onChange={priceInput(setPerK)}
                  placeholder="0"
                />
              )}
            </div>
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
