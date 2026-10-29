import { useCallback, useEffect, useRef, useState } from "react";
import { api, setListParam, type AwsRegion, type Credential, type CredentialInput, type CredentialSettings, type Provider, type Secrets } from "../api";
import { Button, FileButton, MultiSelect, Segmented, Select, TextInput, useDebounced } from "../components/controls";
import { Badge, ConfirmDialog, EditIcon, ErrorBanner, IconButton, Modal, type ConfirmRequest } from "../components/ui";
import { dateTime } from "../format";
import { MISTRAL_REGIONS, PROVIDERS, PROVIDER_LABEL, VERTEX_REGIONS } from "../providers";

type BedrockAuth = "chain" | "keys" | "api_key";
type AzureAuth = "api_key" | "entra";

/** An Azure credential that signs in with an Entra ID app registration instead of an API key. */
export const usesEntra = (c: Credential) => c.provider === "azure" && !!c.settings.tenant_id;

function authSummary(c: Credential): string {
  if (c.provider === "bedrock") {
    if (c.secrets_set.api_key) return "Bedrock API key";
    if (c.secrets_set.aws_access_key_id) return `AWS access keys${c.secrets_set.aws_session_token ? " + session token" : ""}`;
    return "AWS credential chain";
  }
  if (c.provider === "vertex") return "Service account";
  if (usesEntra(c)) return "Entra ID app";
  return c.secrets_set.api_key ? "API key" : "No authentication";
}

/** A credential that cannot work yet, for example one created by an import, which brings no secrets. */
function missingSecret(c: Credential): boolean {
  if (c.provider === "vertex") return !c.secrets_set.google_service_account;
  if (usesEntra(c)) return !c.secrets_set.client_secret;
  // OpenAI-compatible servers may need no key, and Bedrock can use the AWS credential chain.
  if (c.provider === "openai" || c.provider === "bedrock") return false;
  return !c.secrets_set.api_key;
}

/** The region label for a region value, or the value itself. */
function regionLabel(regions: { value: string; label: string }[], value: string | undefined): string {
  const v = value || "global";
  return regions.find((r) => r.value === v)?.label ?? v;
}

/** What a service account key file says about itself, for display before it is saved. */
interface KeyFileInfo {
  file: string;
  email: string;
  project: string | null;
}

function readKeyFile(name: string, text: string): KeyFileInfo {
  let j: Record<string, unknown>;
  try {
    j = JSON.parse(text);
  } catch {
    throw new Error(`${name} is not a JSON file.`);
  }
  if (j.type !== "service_account" || typeof j.client_email !== "string" || typeof j.private_key !== "string") {
    throw new Error(`${name} is not a service account key file (type service_account with client_email and private_key).`);
  }
  return { file: name, email: j.client_email, project: typeof j.project_id === "string" ? j.project_id : null };
}

function host(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

export function connectionSummary(c: Credential): string {
  const s = c.settings;
  switch (c.provider) {
    case "azure":
      return `${s.base_url ? host(s.base_url) : "endpoint missing"}${s.api_version ? ` (api-version ${s.api_version})` : ""}`;
    case "bedrock":
      return `${s.region ?? "region from AWS environment"}${s.base_url ? ` via ${host(s.base_url)}` : ""}`;
    case "openai":
      return s.base_url ? host(s.base_url) : "api.openai.com";
    case "anthropic":
      return s.base_url ? host(s.base_url) : "api.anthropic.com";
    case "typesafe":
      return s.base_url ? host(s.base_url) : "api.typesafe.ai";
    case "mistral":
      return s.base_url ? host(s.base_url) : regionLabel(MISTRAL_REGIONS, s.region);
    case "gemini":
      return s.base_url ? host(s.base_url) : "generativelanguage.googleapis.com";
    case "vertex": {
      const project = s.project_id ?? c.service_account?.project_id ?? "project missing";
      return `${project} · ${regionLabel(VERTEX_REGIONS, s.region)}${s.base_url ? ` via ${host(s.base_url)}` : ""}`;
    }
  }
}

/** The roles an Entra ID app needs, assigned on the resource (or its resource group). */
const ENTRA_PERMISSIONS = (
  <ul className="hint-list">
    <li>An app registration&apos;s client credentials, used instead of the resource&apos;s key. Assign its roles on the resource or its resource group.</li>
    <li>
      <strong>Model calls:</strong> Cognitive Services OpenAI User for Azure OpenAI models, or Cognitive Services User for every Foundry model
      (also Claude, Cohere rerank and Microsoft-Decision).
    </li>
    <li>
      <strong>Deployment list with RPM and TPM</strong> when adding a deployment: read access to the resource in Azure Resource Manager.
      Cognitive Services User includes it; with only Cognitive Services OpenAI User, add Reader. Without it, type the values in.
    </li>
  </ul>
);

/** AWS partitions in the order the region list shows them, and their names. */
const AWS_PARTITIONS = ["aws", "aws-eusc", "aws-us-gov", "aws-cn"];
const AWS_PARTITION_LABEL: Record<string, string> = {
  aws: "AWS",
  "aws-eusc": "AWS European Sovereign Cloud",
  "aws-us-gov": "AWS GovCloud (US)",
  "aws-cn": "AWS China",
};

/** The IAM permissions a Bedrock credential needs, for what. */
const BEDROCK_PERMISSIONS = (
  <ul className="hint-list">
    <li>
      <strong>Model calls:</strong> <span className="mono">bedrock:InvokeModel</span> and{" "}
      <span className="mono">bedrock:InvokeModelWithResponseStream</span>, also used by Converse; for rerank models also{" "}
      <span className="mono">bedrock:Rerank</span>; for token counting with Claude via InvokeModel also{" "}
      <span className="mono">bedrock:CountTokens</span>. For an inference profile, on the profile and on its model in every region it routes
      to.
    </li>
    <li>
      <strong>Model list</strong> when adding a deployment: <span className="mono">bedrock:ListFoundationModels</span> and{" "}
      <span className="mono">bedrock:ListInferenceProfiles</span>.
    </li>
    <li>
      <strong>RPM and TPM from the account&apos;s quotas:</strong> <span className="mono">servicequotas:ListServiceQuotas</span>.
    </li>
    <li>
      <strong>Guardrails</strong> using this credential: <span className="mono">bedrock:ApplyGuardrail</span> on the guardrail.
    </li>
    <li>Without the list permissions, or with a Bedrock API key, which cannot list, type the values in.</li>
  </ul>
);

/** The partition of a region, for ARNs: from the SDK's list, else from the region's name. */
function awsPartition(region: string, regions: AwsRegion[] | null): string {
  const known = regions?.find((r) => r.id === region)?.partition;
  if (known) return known;
  if (region.startsWith("cn-")) return "aws-cn";
  if (region.startsWith("us-gov-")) return "aws-us-gov";
  if (region.startsWith("eusc-")) return "aws-eusc";
  return "aws";
}

/**
 * An IAM policy with everything the gateway does with a Bedrock credential. Inference profiles are
 * called in the credential's region and route to the model in other regions (a global profile to
 * any region, whose model ARN has no region), so models are allowed in every region. Rerank and the
 * listings take no resource; guardrails may live in another region (a guardrail ARN names it).
 */
export function bedrockPolicy(region: string, partition: string) {
  const arn = (rest: string) => `arn:${partition}:bedrock:${rest}`;
  const here = region || "*";
  return {
    Version: "2012-10-17",
    Statement: [
      {
        Sid: "GatewayModelCalls",
        Effect: "Allow",
        Action: ["bedrock:InvokeModel", "bedrock:InvokeModelWithResponseStream", "bedrock:CountTokens"],
        Resource: [arn("*::foundation-model/*"), arn(`${here}:*:inference-profile/*`), arn(`${here}:*:application-inference-profile/*`)],
      },
      { Sid: "GatewayRerank", Effect: "Allow", Action: ["bedrock:Rerank"], Resource: "*" },
      { Sid: "GatewayGuardrails", Effect: "Allow", Action: ["bedrock:ApplyGuardrail"], Resource: arn("*:*:guardrail/*") },
      { Sid: "ConsoleModelList", Effect: "Allow", Action: ["bedrock:ListFoundationModels", "bedrock:ListInferenceProfiles"], Resource: "*" },
      { Sid: "ConsoleQuotas", Effect: "Allow", Action: ["servicequotas:ListServiceQuotas"], Resource: "*" },
    ],
  };
}

/** The IAM policy for a Bedrock credential, with a Copy button. */
function BedrockPolicyDialog({ region, partition, onClose }: { region: string; partition: string; onClose: () => void }) {
  const [copied, setCopied] = useState<"yes" | "select" | null>(null);
  const text = useRef<HTMLPreElement>(null);
  const policy = JSON.stringify(bedrockPolicy(region, partition), null, 2);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(policy);
      setCopied("yes");
    } catch {
      // The browser refused (no clipboard permission): select the policy for Ctrl+C instead.
      const range = document.createRange();
      if (text.current) range.selectNodeContents(text.current);
      window.getSelection()?.removeAllRanges();
      window.getSelection()?.addRange(range);
      setCopied("select");
    }
  };
  return (
    <Modal
      title="IAM policy for Bedrock"
      size="wide"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Close</Button>
          <Button variant="primary" onClick={copy}>
            {copied === "yes" ? "Copied" : "Copy"}
          </Button>
        </>
      }
    >
      <ul className="hint-list">
        <li>
          Attach it to the IAM user of the access keys, or to the role the gateway runs as (IRSA, Pod Identity, ECS task role, instance
          profile). A Bedrock API key brings its own permissions.
        </li>
        <li>
          <strong>Model calls</strong> (Converse, Claude via InvokeModel, token counting, Titan embeddings): models in every region, since an
          inference profile in {region || "the credential's region"} routes requests to other regions, and a global one to any.
        </li>
        <li>
          <strong>Rerank</strong> and <strong>guardrails</strong>, if you use them. <strong>Console</strong>: the model list and the RPM and
          TPM quotas when adding a deployment; leave these out to type the values in.
        </li>
        <li>To narrow it down, replace the <code>*</code> after <code>foundation-model/</code> and the profiles with the models you use.</li>
      </ul>
      {copied === "select" && <p className="muted small">The browser does not allow copying here. The policy is selected: press Ctrl+C (⌘C).</p>}
      <pre className="raw" ref={text}>
        {policy}
      </pre>
    </Modal>
  );
}

export function Credentials() {
  const [creds, setCreds] = useState<Credential[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Credential | "new" | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);
  const [nameFilter, setNameFilter] = useState("");
  const [providerFilter, setProviderFilter] = useState<Provider[]>([]);
  const search = useDebounced(nameFilter.trim());
  const filtering = !!search || providerFilter.length > 0;
  // The server filters the credentials.
  const query = new URLSearchParams();
  if (search) query.set("q", search);
  setListParam(query, "provider", providerFilter);
  const filters = query.toString().replace(/%2C/g, ",");

  const load = useCallback(async () => {
    try {
      setCreds(await api<Credential[]>(`/credentials${filters ? `?${filters}` : ""}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [filters]);

  useEffect(() => {
    load();
  }, [load]);

  const remove = (c: Credential) =>
    setConfirming({
      title: "Delete credential",
      message: (
        <>
          Delete credential <strong>{c.name}</strong>? Its stored keys are removed permanently.
        </>
      ),
      action: async () => {
        await api(`/credentials/${c.id}`, { method: "DELETE" });
        await load();
      },
    });

  const shown = creds;

  return (
    <div className="page">
      <div className="page-head">
        {(creds.length > 0 || filtering) && (
          <div className="filters">
            <TextInput caption="Name" type="search" value={nameFilter} onChange={(e) => setNameFilter(e.target.value)} placeholder="filter" />
            <MultiSelect
              caption="Providers"
              values={providerFilter}
              onChange={(v) => setProviderFilter(v as Provider[])}
              options={PROVIDERS.map((p) => ({ value: p, label: PROVIDER_LABEL[p] }))}
            />
            {filtering && (
              <span className="muted small">
                {shown.length} {shown.length === 1 ? "credential matches" : "credentials match"}
              </span>
            )}
          </div>
        )}
        <button className="primary" onClick={() => setEditing("new")}>
          New credential
        </button>
      </div>
      <ErrorBanner error={error} />
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Provider</th>
              <th>Endpoint / region</th>
              <th>Authentication</th>
              <th className="num">Used by</th>
              <th>Updated</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {shown.map((c) => (
              <tr key={c.id}>
                <td className="mono">{c.name}</td>
                <td>{PROVIDER_LABEL[c.provider]}</td>
                <td className="mono">{connectionSummary(c)}</td>
                <td title={c.service_account?.client_email}>{missingSecret(c) ? <Badge tone="warning">Secret needed: edit to enter it</Badge> : authSummary(c)}</td>
                <td className="num">
                  {c.deployments ? `${c.deployments} deployment${c.deployments === 1 ? "" : "s"}` : <Badge tone="neutral">Unused</Badge>}
                </td>
                <td className="when">{dateTime(c.updated_at)}</td>
                <td className="actions">
                  <IconButton label={`Edit ${c.name}`} title="Edit" onClick={() => setEditing(c)}>
                    <EditIcon />
                  </IconButton>
                </td>
              </tr>
            ))}
            {shown.length === 0 && (
              <tr>
                <td colSpan={7} className="empty">
                  {filtering ? "No credentials match the filters." : "No credentials yet. Create one to start adding models."}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
      {editing && (
        <CredentialForm
          initial={editing === "new" ? undefined : editing}
          onClose={() => setEditing(null)}
          onDelete={
            editing === "new"
              ? undefined
              : () => {
                  setEditing(null);
                  remove(editing);
                }
          }
          onSave={async (input) => {
            if (editing === "new") await api("/credentials", { method: "POST", json: input });
            else await api(`/credentials/${editing.id}`, { method: "PUT", json: input });
            setEditing(null);
            await load();
          }}
        />
      )}
    </div>
  );
}

function initialBedrockAuth(c?: Credential): BedrockAuth {
  if (c?.secrets_set.api_key) return "api_key";
  if (c?.secrets_set.aws_access_key_id) return "keys";
  return "chain";
}

function CredentialForm({
  initial,
  onSave,
  onDelete,
  onClose,
}: {
  initial?: Credential;
  onSave: (c: CredentialInput) => Promise<void>;
  /** Offered when editing: closes the dialog and asks for confirmation. Disabled while deployments use the credential. */
  onDelete?: () => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [provider, setProvider] = useState<Provider>(initial?.provider ?? "openai");
  const [bedrockAuth, setBedrockAuth] = useState<BedrockAuth>(initialBedrockAuth(initial));
  const [baseUrl, setBaseUrl] = useState(initial?.settings.base_url ?? "");
  const [apiVersion, setApiVersion] = useState(initial?.settings.api_version ?? "");
  const [region, setRegion] = useState(initial?.settings.region ?? "");
  // Vertex: a single region typed in, rather than one of the named locations.
  // Bedrock: the regions of the gateway's AWS SDK, loaded once; a region the SDK does not list
  // (newer than the gateway) is typed in.
  const [awsRegions, setAwsRegions] = useState<AwsRegion[] | null>(null);
  const [awsOther, setAwsOther] = useState(false);
  const [showPolicy, setShowPolicy] = useState(false);
  useEffect(() => {
    if (provider !== "bedrock" || awsRegions) return;
    api<AwsRegion[]>("/aws-regions")
      .then(setAwsRegions)
      .catch(() => setAwsRegions([]));
  }, [provider, awsRegions]);
  const regionUnlisted = !!region && !!awsRegions && !awsRegions.some((r) => r.id === region);
  const [vertexOther, setVertexOther] = useState(
    initial?.provider === "vertex" && !!initial.settings.region && !VERTEX_REGIONS.some((r) => r.value === initial.settings.region),
  );
  const [projectId, setProjectId] = useState(initial?.settings.project_id ?? "");
  const [authHeader, setAuthHeader] = useState(initial?.settings.auth_header ?? "");
  const [azureAuth, setAzureAuth] = useState<AzureAuth>(initial && usesEntra(initial) ? "entra" : "api_key");
  const [tenantId, setTenantId] = useState(initial?.settings.tenant_id ?? "");
  const [clientId, setClientId] = useState(initial?.settings.client_id ?? "");
  const [keyFile, setKeyFile] = useState<KeyFileInfo | null>(null);
  const [secrets, setSecrets] = useState<Secrets>({});
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const set = (k: keyof Secrets) => (e: { target: { value: string } }) => setSecrets((s) => ({ ...s, [k]: e.target.value }));
  const placeholder = (k: keyof Secrets, fallback = "") => {
    if (!initial?.secrets_set[k]) return fallback;
    return "stored — leave blank to keep";
  };

  const changeProvider = (p: Provider) => {
    setProvider(p);
    // Regions mean something different for each provider.
    setRegion("");
    setVertexOther(false);
    setAwsOther(false);
  };

  const loadKeyFile = async (file: File | undefined) => {
    if (!file) return;
    try {
      const text = await file.text();
      setKeyFile(readKeyFile(file.name, text));
      setSecrets((s) => ({ ...s, google_service_account: text }));
      setError(null);
    } catch (e) {
      setKeyFile(null);
      setSecrets(({ google_service_account: _, ...rest }) => rest);
      setError((e as Error).message);
    }
  };

  const submit = async () => {
    setBusy(true);
    try {
      // Only send what the user typed for this provider; on edit, untouched fields keep their stored values.
      // Fields hidden by a provider switch (or filled in by the browser) still hold values, so drop those.
      const applies: (keyof Secrets)[] =
        provider === "vertex"
          ? ["google_service_account"]
          : provider === "bedrock"
            ? bedrockAuth === "api_key"
              ? ["api_key"]
              : bedrockAuth === "keys"
                ? ["aws_access_key_id", "aws_secret_access_key", "aws_session_token"]
                : []
            : provider === "azure" && azureAuth === "entra"
              ? ["client_secret"]
              : ["api_key"];
      const out: Secrets = Object.fromEntries(
        Object.entries(secrets).filter(([k, v]) => v !== "" && applies.includes(k as keyof Secrets)),
      );
      if (provider === "bedrock" && initial) {
        // Switching auth mode clears the secrets of the other modes.
        if (bedrockAuth !== "api_key") out.api_key = "";
        if (bedrockAuth !== "keys") Object.assign(out, { aws_access_key_id: "", aws_secret_access_key: "", aws_session_token: "" });
      }
      // Likewise for Azure: an API key or the app's client secret, never both.
      if (provider === "azure" && initial) {
        if (azureAuth === "entra" && initial.secrets_set.api_key) out.api_key = "";
        if (azureAuth === "api_key" && initial.secrets_set.client_secret) out.client_secret = "";
      }
      const entra = provider === "azure" && azureAuth === "entra";
      // Settings are not secret and always sent in full; only those that apply to the provider.
      const settings: CredentialSettings = {
        ...(baseUrl.trim() && { base_url: baseUrl.trim() }),
        ...(provider === "azure" && apiVersion.trim() && { api_version: apiVersion.trim() }),
        ...(hasRegion && region.trim() && { region: region.trim() }),
        ...(provider === "vertex" && projectId.trim() && { project_id: projectId.trim() }),
        ...(provider === "openai" && authHeader.trim() && { auth_header: authHeader.trim() }),
        ...(entra && { tenant_id: tenantId.trim(), client_id: clientId.trim() }),
      };
      await onSave({ name: name.trim(), provider, settings, secrets: out });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const hasRegion = provider === "bedrock" || provider === "mistral" || provider === "vertex";
  const entraAuth = provider === "azure" && azureAuth === "entra";
  const keyRequired = ["azure", "anthropic", "typesafe", "mistral", "gemini"].includes(provider) && !entraAuth && !initial?.secrets_set.api_key;
  const secretRequired = entraAuth && !initial?.secrets_set.client_secret;
  const storedAccount = initial?.service_account;
  const missing =
    !name.trim() ||
    (provider === "azure" && !baseUrl.trim()) ||
    (provider === "vertex" && !initial?.secrets_set.google_service_account && !secrets.google_service_account) ||
    (provider === "vertex" && vertexOther && !region.trim()) ||
    (provider === "bedrock" && awsOther && !region.trim()) ||
    (keyRequired && !secrets.api_key) ||
    (entraAuth && (!tenantId.trim() || !clientId.trim())) ||
    (secretRequired && !secrets.client_secret) ||
    (provider === "bedrock" && bedrockAuth === "api_key" && !initial?.secrets_set.api_key && !secrets.api_key) ||
    (provider === "bedrock" && bedrockAuth === "keys" && !initial?.secrets_set.aws_access_key_id && (!secrets.aws_access_key_id || !secrets.aws_secret_access_key));

  return (
    <Modal
      title={initial ? "Edit credential" : "New credential"}
      onClose={onClose}
      size="medium"
      footer={
        <>
          {onDelete && (
            <Button
              variant="danger"
              onClick={onDelete}
              disabled={!!initial?.deployments}
              title={initial?.deployments ? "In use: remove or reassign its deployments first" : undefined}
            >
              Delete
            </Button>
          )}
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy || missing} onClick={submit}>
            {initial ? "Save" : "Create credential"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} />
      <div className="form">
        <TextInput caption="Name" value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. azure-prod-westeurope" required autoFocus />
        <Select
          caption="Provider"
          hint={initial ? "Fixed after creation." : undefined}
          value={provider}
          disabled={!!initial}
          onChange={(p) => changeProvider(p as Provider)}
          options={PROVIDERS.map((p) => ({ value: p, label: PROVIDER_LABEL[p] }))}
        />

        {/* URLs get a line of their own, so a typical one fits without scrolling. */}
        {provider === "azure" && (
          <>
            <TextInput
              caption="Resource endpoint"
              hint="/openai/v1, /anthropic/v1 or the decisions route is added to match each deployment's protocol."
              mono
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder="e.g. https://my-resource.services.ai.azure.com"
              required
            />
            <TextInput caption="api-version" hint="Optional; the v1 API needs none." value={apiVersion} onChange={(e) => setApiVersion(e.target.value)} />
          </>
        )}
        {provider === "bedrock" && (
          <>
            {awsRegions?.length ? (
              <div className="row">
                <Select
                  caption="Region"
                  hint="The regions of the AWS SDK this gateway was built with. From the AWS environment uses the region of the gateway's environment (AWS_REGION, profile or instance)."
                  value={awsOther || regionUnlisted ? "other" : region}
                  onChange={(v) => {
                    const other = v === "other";
                    setAwsOther(other);
                    setRegion(other ? "" : v);
                  }}
                  options={[
                    { value: "", label: "From the AWS environment" },
                    ...[...awsRegions]
                      .sort((a, b) => AWS_PARTITIONS.indexOf(a.partition) - AWS_PARTITIONS.indexOf(b.partition) || a.name.localeCompare(b.name))
                      .map((r) => ({ value: r.id, label: r.name, note: r.id, group: AWS_PARTITION_LABEL[r.partition] ?? r.partition })),
                    { value: "other", label: "Other region…", group: "Other" },
                  ]}
                />
                {(awsOther || regionUnlisted) && (
                  <TextInput caption="Region" hint="A region this gateway's SDK does not list yet." mono value={region} onChange={(e) => setRegion(e.target.value)} placeholder="e.g. eu-central-1" required />
                )}
              </div>
            ) : (
              <TextInput caption="Region" hint="Leave empty to use the AWS environment's region." value={region} onChange={(e) => setRegion(e.target.value)} placeholder="from the AWS environment" />
            )}
            <TextInput
              caption="Endpoint override"
              hint="Optional, for example a VPC endpoint."
              mono
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder="e.g. https://bedrock-runtime.us-east-1.amazonaws.com"
            />
          </>
        )}
        {provider === "mistral" && (
          <>
            <Select
              caption="Region"
              hint="EU and US pin processing to that region; they serve fewer models and cost 10% more."
              value={region || "global"}
              onChange={setRegion}
              options={MISTRAL_REGIONS}
            />
            <TextInput
              caption="Endpoint override"
              hint="Optional. Replaces the regional endpoint. Include the /v1 path."
              mono
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder="https://api.mistral.ai/v1"
            />
          </>
        )}
        {provider === "vertex" && (
          <>
            <div className="row">
              <Select
                caption="Location"
                hint="Global routes anywhere. The EU and US multi-regions keep processing in that geography."
                value={vertexOther ? "other" : region || "global"}
                onChange={(v) => {
                  const other = v === "other";
                  setVertexOther(other);
                  setRegion(other ? "" : v);
                }}
                options={[...VERTEX_REGIONS, { value: "other", label: "Single region…" }]}
              />
              {vertexOther && (
                <TextInput caption="Region" hint="Check that the region serves your models." value={region} onChange={(e) => setRegion(e.target.value)} placeholder="e.g. europe-west4" required />
              )}
            </div>
            <TextInput
              caption="Project ID"
              hint="Leave empty to use the service account's project."
              mono
              value={projectId}
              onChange={(e) => setProjectId(e.target.value)}
              placeholder={keyFile?.project ?? storedAccount?.project_id ?? "from the key file"}
            />
            <TextInput
              caption="Endpoint override"
              hint="Optional, for example a Private Service Connect endpoint. Without a path."
              mono
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder="https://aiplatform.googleapis.com"
            />
          </>
        )}
        {(provider === "openai" || provider === "anthropic" || provider === "typesafe" || provider === "gemini") && (
          <TextInput
            caption="Base URL"
            hint={
              provider === "openai"
                ? "Any OpenAI-compatible server (vLLM, Ollama, …). Include the /v1 path."
                : provider === "anthropic"
                  ? "Leave empty for the Anthropic API."
                  : provider === "gemini"
                    ? "Leave empty for the Gemini API's OpenAI-compatible endpoint. It has no regional endpoints."
                    : "Leave empty for the TypeSafe API."
            }
            mono
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder={
              provider === "openai"
                ? "https://api.openai.com/v1"
                : provider === "anthropic"
                  ? "https://api.anthropic.com/v1"
                  : provider === "gemini"
                    ? "https://generativelanguage.googleapis.com/v1beta/openai"
                    : "https://api.typesafe.ai"
            }
          />
        )}

        {provider === "vertex" && (
          <div className="field">
            <FileButton
              caption="Service account key"
              hint="The JSON key file from Google Cloud (IAM → Service accounts → Keys). The account needs the Vertex AI User role."
              accept=".json,application/json"
              onFile={loadKeyFile}
              required={!initial?.secrets_set.google_service_account && !secrets.google_service_account}
            >
              {keyFile || storedAccount ? "Replace key file…" : "Choose key file…"}
            </FileButton>
            {keyFile ? (
              <p className="muted small">
                {keyFile.file}: <span className="mono">{keyFile.email}</span>
                {keyFile.project && <> in project <span className="mono">{keyFile.project}</span></>}
                {initial?.secrets_set.google_service_account && " (replaces the stored key)"}
              </p>
            ) : (
              storedAccount && (
                <p className="muted small">
                  Stored: <span className="mono">{storedAccount.client_email}</span>. Choose a file only to replace it.
                </p>
              )
            )}
          </div>
        )}

        {provider === "bedrock" && (
          <div className="row with-action">
            <Select
              caption="Authentication"
              hint={BEDROCK_PERMISSIONS}
              value={bedrockAuth}
              onChange={(a) => setBedrockAuth(a as BedrockAuth)}
              options={[
                { value: "chain", label: "AWS credential chain (IAM role: IRSA, Pod Identity, ECS task role, instance profile)" },
                { value: "keys", label: "Access key pair" },
                { value: "api_key", label: "Bedrock API key" },
              ]}
            />
            <Button title="The IAM permissions everything here needs, to copy" onClick={() => setShowPolicy(true)}>
              IAM policy
            </Button>
            {showPolicy && <BedrockPolicyDialog region={region} partition={awsPartition(region, awsRegions)} onClose={() => setShowPolicy(false)} />}
          </div>
        )}

        {provider === "azure" && (
          <Segmented
            caption="Authentication"
            hint={azureAuth === "entra" ? ENTRA_PERMISSIONS : "The resource's key (Keys and Endpoint in the Azure portal)."}
            value={azureAuth}
            onChange={setAzureAuth}
            options={[
              { value: "api_key", label: "API key" },
              { value: "entra", label: "Entra ID" },
            ]}
          />
        )}
        {entraAuth && (
          <>
            <div className="row">
              <TextInput
                caption="Tenant ID"
                hint="Directory (tenant) ID of the app registration, or a domain such as contoso.onmicrosoft.com."
                mono
                autoComplete="off"
                value={tenantId}
                onChange={(e) => setTenantId(e.target.value)}
                placeholder="00000000-0000-0000-0000-000000000000"
                required
              />
              <TextInput
                caption="Client ID"
                hint="Application (client) ID of the app registration."
                mono
                autoComplete="off"
                value={clientId}
                onChange={(e) => setClientId(e.target.value)}
                placeholder="00000000-0000-0000-0000-000000000000"
                required
              />
            </div>
            <TextInput
              caption="Client secret"
              hint="A secret's value from the app registration (Certificates & secrets), not its ID."
              secret
              value={secrets.client_secret ?? ""}
              onChange={set("client_secret")}
              placeholder={placeholder("client_secret")}
              required={secretRequired}
            />
          </>
        )}

        {provider !== "vertex" && !entraAuth && (provider !== "bedrock" || bedrockAuth === "api_key") && (
          <TextInput
            caption={provider === "bedrock" ? "Bedrock API key" : "API key"}
            hint={provider === "openai" ? "Leave empty for servers without authentication." : undefined}
            secret
            value={secrets.api_key ?? ""}
            onChange={set("api_key")}
            placeholder={placeholder("api_key")}
            required={provider === "bedrock" ? !initial?.secrets_set.api_key : keyRequired}
          />
        )}
        {provider === "openai" && (
          <TextInput
            caption="API key header"
            hint="For servers that take the key in another header. Leave empty to send it as Authorization: Bearer."
            mono
            autoComplete="off"
            value={authHeader}
            onChange={(e) => setAuthHeader(e.target.value)}
            placeholder="Authorization: Bearer"
          />
        )}

        {provider === "bedrock" && bedrockAuth === "keys" && (
          <>
            <div className="row">
              <TextInput
                caption="Access key ID"
                mono
                autoComplete="off"
                value={secrets.aws_access_key_id ?? ""}
                onChange={set("aws_access_key_id")}
                placeholder={placeholder("aws_access_key_id")}
                required={!initial?.secrets_set.aws_access_key_id}
              />
              <TextInput
                caption="Secret access key"
                secret
                value={secrets.aws_secret_access_key ?? ""}
                onChange={set("aws_secret_access_key")}
                placeholder={placeholder("aws_secret_access_key")}
                required={!initial?.secrets_set.aws_access_key_id}
              />
            </div>
            <TextInput
              caption="Session token (optional)"
              secret
              value={secrets.aws_session_token ?? ""}
              onChange={set("aws_session_token")}
              placeholder={placeholder("aws_session_token")}
            />
          </>
        )}

        {provider === "bedrock" && bedrockAuth === "chain" && (
          <p className="muted small">
            No secret is stored. The gateway uses the AWS SDK's default credential chain at request time.
          </p>
        )}

        <p className="muted small form-note">Values are encrypted at rest in the database and never shown again.</p>
      </div>
    </Modal>
  );
}
