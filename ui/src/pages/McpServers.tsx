import { useCallback, useEffect, useState } from "react";
import { api, type McpAuth, type McpServer, type McpServerInput, type McpServerTestResult, type NamedItem, type TagInfo } from "../api";
import { Button, MenuButton, MultiSelect, PageActions, Segmented, TextInput, Toggle } from "../components/controls";
import { TagEditor, TagList } from "../components/Tags";
import { Badge, ConfirmDialog, EditIcon, ErrorBanner, IconButton, MenuIcon, Modal, TryIcon, type ConfirmRequest } from "../components/ui";
import { dateTime, ms } from "../format";

const AUTH_LABEL: Record<McpAuth, string> = { none: "None", bearer: "Bearer token", header: "Header" };

/** Where clients reach a server through the gateway. */
const endpoint = (name: string) => `${location.origin}/mcp/${name}`;

/** The upstream's host and path, without the scheme. */
const upstream = (url: string) => url.replace(/^https?:\/\//, "");

/** MCP servers: upstreams the gateway proxies at `/mcp/{name}`, with its own credentials for them. */
export function McpServers() {
  const [rows, setRows] = useState<McpServer[]>([]);
  const [projects, setProjects] = useState<NamedItem[]>([]);
  const [tags, setTags] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<McpServer | "new" | null>(null);
  const [copyOf, setCopyOf] = useState<McpServer | null>(null);
  const [trying, setTrying] = useState<McpServer | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);

  const load = useCallback(async () => {
    try {
      const [s, p, t] = await Promise.all([api<McpServer[]>("/mcp-servers"), api<NamedItem[]>("/projects/options"), api<TagInfo[]>("/tags")]);
      setRows(s);
      setProjects(p);
      setTags(t.map((x) => x.tag));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const save = async (s: McpServer, changes: Partial<McpServerInput>) => {
    try {
      const { name, description, url, auth, auth_header, tags, project_ids, timeout_secs, enabled } = s;
      await api(`/mcp-servers/${s.id}`, { method: "PUT", json: { name, description, url, auth, auth_header, tags, project_ids, timeout_secs, enabled, ...changes } });
      await load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const remove = (s: McpServer) =>
    setConfirming({
      title: "Delete MCP server",
      message: (
        <>
          Delete MCP server <strong>{s.name}</strong>? Clients using <span className="mono">/mcp/{s.name}</span> stop working. The server itself is not changed.
        </>
      ),
      action: async () => {
        await api(`/mcp-servers/${s.id}`, { method: "DELETE" });
        await load();
      },
    });

  const projectNames = (ids: string[]) => ids.map((id) => projects.find((p) => p.id === id)?.name ?? "unknown project");

  return (
    <div className="page">
      <div className="page-head">
        <p className="muted small">
          Clients connect to <span className="mono">{endpoint("{name}")}</span> over Streamable HTTP, with a virtual key as their bearer token.
        </p>
        <PageActions actions={[{ label: "New MCP server", onClick: () => setEditing("new"), primary: true }]} />
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Server</th>
              <th>Authentication</th>
              <th>Access</th>
              <th>Status</th>
              <th>Updated</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((s) => (
              <tr key={s.id}>
                <td>
                  <span className="mono">{s.name}</span>
                  {s.description && <div className="muted small">{s.description}</div>}
                </td>
                <td className="mono clip" title={s.url}>
                  {upstream(s.url)}
                </td>
                <td>
                  {AUTH_LABEL[s.auth]}
                  {s.auth === "header" && <span className="muted small mono"> {s.auth_header}</span>}
                </td>
                <td>
                  <TagList tags={s.tags} projects={projectNames(s.project_ids)} empty="Every key" />
                </td>
                <td>{s.enabled ? <Badge tone="good">Enabled</Badge> : <Badge tone="neutral">Disabled</Badge>}</td>
                <td className="when">{dateTime(s.updated_at)}</td>
                <td className="actions">
                  <IconButton label={`Edit ${s.name}`} title="Edit" onClick={() => setEditing(s)}>
                    <EditIcon />
                  </IconButton>
                  <IconButton label={`Try ${s.name}`} title="Connect and list its tools" onClick={() => setTrying(s)}>
                    <TryIcon />
                  </IconButton>
                  <MenuButton
                    label={`More for ${s.name}`}
                    className="icon-btn"
                    items={[
                      { label: s.enabled ? "Disable" : "Enable", onClick: () => save(s, { enabled: !s.enabled }) },
                      {
                        label: "Duplicate",
                        onClick: () => {
                          setCopyOf(s);
                          setEditing("new");
                        },
                      },
                      { label: "Delete", danger: true, onClick: () => remove(s) },
                    ]}
                  >
                    <MenuIcon />
                  </MenuButton>
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={7} className="empty">
                  No MCP servers yet.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
      {trying && <TryMcpServer server={trying} onClose={() => setTrying(null)} />}
      {editing && (
        <McpServerForm
          initial={editing === "new" ? undefined : editing}
          copyOf={editing === "new" ? (copyOf ?? undefined) : undefined}
          projects={projects}
          knownTags={tags}
          onClose={() => {
            setEditing(null);
            setCopyOf(null);
          }}
          onSave={async (input) => {
            if (editing === "new") await api("/mcp-servers", { method: "POST", json: input });
            else await api(`/mcp-servers/${editing.id}`, { method: "PUT", json: input });
            setEditing(null);
            setCopyOf(null);
            await load();
          }}
        />
      )}
    </div>
  );
}

function McpServerForm({
  initial,
  copyOf,
  projects,
  knownTags,
  onSave,
  onClose,
}: {
  initial?: McpServer;
  /** Duplicate: a new server starting from this one, without its secret. */
  copyOf?: McpServer;
  projects: NamedItem[];
  knownTags: string[];
  onSave: (s: McpServerInput) => Promise<void>;
  onClose: () => void;
}) {
  const from = initial ?? copyOf;
  const [name, setName] = useState(initial?.name ?? (copyOf ? `${copyOf.name}-copy` : ""));
  const [description, setDescription] = useState(from?.description ?? "");
  const [url, setUrl] = useState(from?.url ?? "");
  const [auth, setAuth] = useState<McpAuth>(from?.auth ?? "bearer");
  const [authHeader, setAuthHeader] = useState(from?.auth_header ?? "x-api-key");
  const [secret, setSecret] = useState("");
  const [tags, setTags] = useState<string[]>(from?.tags ?? []);
  const [projectIds, setProjectIds] = useState<string[]>(from?.project_ids ?? []);
  const [timeout, setTimeoutSecs] = useState(String(from?.timeout_secs ?? 300));
  const [enabled, setEnabled] = useState(from?.enabled ?? true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // A stored secret stays unless a new one is typed; a duplicate has none.
  const keepsSecret = !!initial?.has_secret && initial.auth !== "none";
  const timeoutSecs = Number(timeout);
  const badTimeout = !Number.isInteger(timeoutSecs) || timeoutSecs < 1 || timeoutSecs > 3600;
  const missing = !name.trim() || !url.trim() || badTimeout || (auth !== "none" && !secret.trim() && !keepsSecret) || (auth === "header" && !authHeader.trim());

  const submit = async () => {
    setBusy(true);
    try {
      await onSave({
        name: name.trim(),
        description: description.trim() || undefined,
        url: url.trim(),
        auth,
        ...(auth === "header" && { auth_header: authHeader.trim() }),
        ...(auth !== "none" && secret.trim() && { secret: secret.trim() }),
        tags,
        project_ids: projectIds,
        timeout_secs: timeoutSecs,
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
      title={initial ? "Edit MCP server" : "New MCP server"}
      onClose={onClose}
      size="medium"
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy || missing} onClick={submit}>
            {initial ? "Save" : "Create MCP server"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="form">
        <TextInput
          caption="Name"
          hint="Clients call the server at /mcp/ followed by this name. Lowercase letters, digits, - and _. Renaming it changes the address clients use."
          mono
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="e.g. github"
          required
          autoFocus
        />
        <TextInput caption="Description" value={description} onChange={(e) => setDescription(e.target.value)} placeholder="optional" />
        <TextInput
          caption="URL"
          hint="The server's Streamable HTTP endpoint, often ending in /mcp. The gateway calls it from its own network: addresses on private networks need GATEWAY_UPSTREAM_ALLOWED_NETWORKS."
          mono
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder="https://mcp.example.com/mcp"
          required
        />
        <Segmented
          caption="Authentication"
          hint="How the gateway signs in to the server. Clients never see this secret: they authenticate to the gateway with their virtual key."
          options={(["none", "bearer", "header"] as const).map((value) => ({ value, label: AUTH_LABEL[value] }))}
          value={auth}
          onChange={setAuth}
        />
        {auth === "header" && (
          <TextInput
            caption="Header"
            hint="The header the server reads its API key from."
            mono
            value={authHeader}
            onChange={(e) => setAuthHeader(e.target.value)}
            placeholder="x-api-key"
            required
          />
        )}
        {auth !== "none" && (
          <TextInput
            caption={auth === "bearer" ? "Token" : "API key"}
            hint={auth === "bearer" ? "Sent as Authorization: Bearer followed by the token. Stored encrypted." : "Sent as the header's value. Stored encrypted."}
            secret
            mono
            value={secret}
            onChange={(e) => setSecret(e.target.value)}
            placeholder={keepsSecret ? "unchanged" : ""}
            required={!keepsSecret}
          />
        )}
        <TagEditor
          caption="Tags"
          hint={
            <ul className="hint-list">
              <li>
                <strong>Keys with tags</strong> can use this server only if they hold every one of its tags.
              </li>
              <li>
                <strong>Keys without tags</strong> can use every server not limited to other projects.
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
                <strong>With projects</strong>, only keys in one of them can use this server. Their tags still apply.
              </li>
              <li>
                <strong>Without</strong>, keys in any project, or in none, can use it as their tags allow.
              </li>
            </ul>
          }
          values={projectIds}
          onChange={setProjectIds}
          options={projects.map((p) => ({ value: p.id, label: p.name }))}
        />
        <div className="row">
          <TextInput
            caption="Timeout"
            hint="Seconds a call may take, its streamed answer included: 1 to 3600."
            inputMode="numeric"
            value={timeout}
            onChange={(e) => setTimeoutSecs(e.target.value)}
          />
          <Toggle caption="Enabled" hint="A disabled server answers 404, as if it did not exist." checked={enabled} onChange={setEnabled} />
        </div>
      </div>
    </Modal>
  );
}

/** Connects to the server through the gateway's admin API and lists its tools, with how to connect a client. */
function TryMcpServer({ server, onClose }: { server: McpServer; onClose: () => void }) {
  const [result, setResult] = useState<McpServerTestResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setResult(await api<McpServerTestResult>(`/mcp-servers/${server.id}/test`, { method: "POST" }));
    } catch (e) {
      setResult(null);
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }, [server.id]);

  useEffect(() => {
    run();
  }, [run]);

  const config = JSON.stringify(
    { mcpServers: { [server.name]: { type: "http", url: endpoint(server.name), headers: { Authorization: "Bearer ${GATEWAY_API_KEY}" } } } },
    null,
    2,
  );
  const info = result?.server_info;
  return (
    <Modal
      title={`Try ${server.name}`}
      size="wide"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Close</Button>
          <Button variant="primary" onClick={run} disabled={busy}>
            {busy ? "Connecting…" : "Connect again"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {result && !result.ok && <ErrorBanner error={`The server could not be used: ${result.error ?? "no details"}`} onDismiss={() => setResult(null)} />}
      {result?.ok && (
        <div className="guardrail-result">
          <div className="guardrail-result-head">
            <Badge tone="good">Connected</Badge>
            <span className="muted small">
              {[info?.name && `${info.name}${info.version ? ` ${info.version}` : ""}`, result.protocol_version && `protocol ${result.protocol_version}`, ms(result.latency_ms)]
                .filter(Boolean)
                .join(" · ")}
            </span>
          </div>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Tool</th>
                  <th>Description</th>
                </tr>
              </thead>
              <tbody>
                {result.tools.map((t) => (
                  <tr key={t.name}>
                    <td className="mono">{t.name}</td>
                    <td className="muted">{t.description}</td>
                  </tr>
                ))}
                {result.tools.length === 0 && (
                  <tr>
                    <td colSpan={2} className="empty">
                      The server offers no tools.
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        </div>
      )}
      <h4>Connect a client</h4>
      <p className="muted small">
        For clients that read an <span className="mono">mcpServers</span> configuration, such as Claude Code, Cursor or VS Code. Replace{" "}
        <span className="mono">{"${GATEWAY_API_KEY}"}</span> with a virtual key{!server.enabled && "; the server is disabled until you enable it"}.
      </p>
      <pre className="raw">{config}</pre>
    </Modal>
  );
}
