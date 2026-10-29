import { useCallback, useEffect, useState } from "react";
import { api, type Credential, type CredentialInput, type CredentialSettings, type Provider, type Secrets } from "../api";
import { Button, FileButton, MultiSelect, Select, TextInput } from "../components/controls";
import { Badge, ConfirmDialog, EditIcon, ErrorBanner, IconButton, Modal, type ConfirmRequest } from "../components/ui";
import { dateTime } from "../format";
import { MISTRAL_REGIONS, PROVIDERS, PROVIDER_LABEL, VERTEX_REGIONS } from "../providers";

type BedrockAuth = "chain" | "keys" | "api_key";

function authSummary(c: Credential): string {
  if (c.provider === "bedrock") {
    if (c.secrets_set.api_key) return "Bedrock API key";
    if (c.secrets_set.aws_access_key_id) return `AWS access keys${c.secrets_set.aws_session_token ? " + session token" : ""}`;
    return "AWS credential chain";
  }
  if (c.provider === "vertex") return "Service account";
  return c.secrets_set.api_key ? "API key" : "No authentication";
}

/** A credential that cannot work yet, for example one created by an import, which brings no secrets. */
function missingSecret(c: Credential): boolean {
  if (c.provider === "vertex") return !c.secrets_set.google_service_account;
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

export function Credentials() {
  const [creds, setCreds] = useState<Credential[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Credential | "new" | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);
  const [nameFilter, setNameFilter] = useState("");
  const [providerFilter, setProviderFilter] = useState<Provider[]>([]);

  const load = useCallback(async () => {
    try {
      setCreds(await api<Credential[]>("/credentials"));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

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

  // Only the providers that have credentials, in the usual order.
  const providers = PROVIDERS.filter((p) => creds.some((c) => c.provider === p));
  const needle = nameFilter.trim().toLowerCase();
  const shown = creds.filter((c) => (!needle || c.name.toLowerCase().includes(needle)) && (!providerFilter.length || providerFilter.includes(c.provider)));
  const filtering = !!needle || providerFilter.length > 0;

  return (
    <div className="page">
      <div className="page-head">
        {creds.length > 0 && (
          <div className="filters">
            <TextInput caption="Name" type="search" value={nameFilter} onChange={(e) => setNameFilter(e.target.value)} placeholder="filter" />
            <MultiSelect
              caption="Providers"
              values={providerFilter}
              onChange={(v) => setProviderFilter(v as Provider[])}
              options={providers.map((p) => ({ value: p, label: PROVIDER_LABEL[p] }))}
            />
            {filtering && (
              <span className="muted small">
                {shown.length} of {creds.length} credentials
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
                  {creds.length ? "No credentials match the filters." : "No credentials yet. Create one to start adding models."}
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
  const [vertexOther, setVertexOther] = useState(
    initial?.provider === "vertex" && !!initial.settings.region && !VERTEX_REGIONS.some((r) => r.value === initial.settings.region),
  );
  const [projectId, setProjectId] = useState(initial?.settings.project_id ?? "");
  const [authHeader, setAuthHeader] = useState(initial?.settings.auth_header ?? "");
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
            : ["api_key"];
      const out: Secrets = Object.fromEntries(
        Object.entries(secrets).filter(([k, v]) => v !== "" && applies.includes(k as keyof Secrets)),
      );
      if (provider === "bedrock" && initial) {
        // Switching auth mode clears the secrets of the other modes.
        if (bedrockAuth !== "api_key") out.api_key = "";
        if (bedrockAuth !== "keys") Object.assign(out, { aws_access_key_id: "", aws_secret_access_key: "", aws_session_token: "" });
      }
      // Settings are not secret and always sent in full; only those that apply to the provider.
      const settings: CredentialSettings = {
        ...(baseUrl.trim() && { base_url: baseUrl.trim() }),
        ...(provider === "azure" && apiVersion.trim() && { api_version: apiVersion.trim() }),
        ...(hasRegion && region.trim() && { region: region.trim() }),
        ...(provider === "vertex" && projectId.trim() && { project_id: projectId.trim() }),
        ...(provider === "openai" && authHeader.trim() && { auth_header: authHeader.trim() }),
      };
      await onSave({ name: name.trim(), provider, settings, secrets: out });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const hasRegion = provider === "bedrock" || provider === "mistral" || provider === "vertex";
  const keyRequired = ["azure", "anthropic", "typesafe", "mistral", "gemini"].includes(provider) && !initial?.secrets_set.api_key;
  const storedAccount = initial?.service_account;
  const missing =
    !name.trim() ||
    (provider === "azure" && !baseUrl.trim()) ||
    (provider === "vertex" && !initial?.secrets_set.google_service_account && !secrets.google_service_account) ||
    (provider === "vertex" && vertexOther && !region.trim()) ||
    (keyRequired && !secrets.api_key) ||
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
            <TextInput caption="Region" hint="Leave empty to use the AWS environment's region." value={region} onChange={(e) => setRegion(e.target.value)} placeholder="from the AWS environment" />
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
          <Select
            caption="Authentication"
            value={bedrockAuth}
            onChange={(a) => setBedrockAuth(a as BedrockAuth)}
            options={[
              { value: "chain", label: "AWS credential chain (IAM role: IRSA, Pod Identity, ECS task role, instance profile)" },
              { value: "keys", label: "Access key pair" },
              { value: "api_key", label: "Bedrock API key" },
            ]}
          />
        )}

        {provider !== "vertex" && (provider !== "bedrock" || bedrockAuth === "api_key") && (
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
