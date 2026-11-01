import { useCallback, useEffect, useState } from "react";
import { api, type McpServer, type McpServerInput, type McpServerTestResult, type NamedItem, type TagInfo } from "../api";
import { Button, MenuButton, MultiSelect, PageActions, Segmented, Switch, TextInput, Toggle } from "../components/controls";
import { TagEditor, TagList } from "../components/Tags";
import { Badge, ConfirmDialog, EditIcon, ErrorBanner, IconButton, MenuIcon, Modal, TryIcon, type ConfirmRequest } from "../components/ui";
import { dateTime, ms, usd } from "../format";
import { AUTH_LABEL, AuthFields, type AuthState, authInput, authMissing, authState, signInPer, withClient } from "./UpstreamAuth";


/** Where clients reach one server through the gateway, or every server with no name. */
const endpoint = (name?: string) => `${location.origin}/mcp${name ? `/${name}` : ""}`;

/** The upstream's host and path, without the scheme. */
const upstream = (url: string) => url.replace(/^https?:\/\//, "");

/** A server as the admin API takes it back, keeping its stored secret. */
const toInput = (s: McpServer): McpServerInput => ({
  name: s.name,
  description: s.description,
  url: s.url,
  auth: s.auth,
  auth_header: s.auth_header,
  oauth: s.oauth,
  tags: s.tags,
  project_ids: s.project_ids,
  timeout_secs: s.timeout_secs,
  enabled: s.enabled,
  allowed_tools: s.allowed_tools,
  denied_tools: s.denied_tools,
  price_per_call: s.price_per_call,
  tool_prices: s.tool_prices,
});

/** Which tools keys may call, short. */
function toolsSummary(s: McpServer): string {
  if (s.allowed_tools.length) return `${s.allowed_tools.length} allowed`;
  if (s.denied_tools.length) return `All but ${s.denied_tools.length}`;
  return "All";
}

/** MCP servers: upstreams the gateway proxies at `/mcp/{name}` and combines at `/mcp`, with its own credentials for them. */
export function McpServers() {
  const [rows, setRows] = useState<McpServer[]>([]);
  const [projects, setProjects] = useState<NamedItem[]>([]);
  const [tags, setTags] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<McpServer | "new" | null>(null);
  const [copyOf, setCopyOf] = useState<McpServer | null>(null);
  const [tools, setTools] = useState<McpServer | null>(null);
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

  const update = async (s: McpServer, changes: Partial<McpServerInput>) => {
    await api(`/mcp-servers/${s.id}`, { method: "PUT", json: { ...toInput(s), ...changes } });
    await load();
  };

  const remove = (s: McpServer) =>
    setConfirming({
      title: "Delete MCP server",
      message: (
        <>
          Delete MCP server <strong>{s.name}</strong>? Clients using <span className="mono">/mcp/{s.name}</span> stop working, and its tools leave{" "}
          <span className="mono">/mcp</span>. The server itself is not changed.
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
          Clients connect to <span className="mono">{endpoint()}</span> for the tools of every server their key may use, named{" "}
          <span className="mono">server__tool</span>, or to <span className="mono">{endpoint("{name}")}</span> for one server. They authenticate with a virtual key.
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
              <th>Tools</th>
              <th>Price</th>
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
                <td>{toolsSummary(s)}</td>
                <td title={Object.keys(s.tool_prices).length ? "Some tools have prices of their own." : undefined}>
                  {usd(s.price_per_call)}
                  {Object.keys(s.tool_prices).length > 0 && <span className="muted small"> +{Object.keys(s.tool_prices).length}</span>}
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
                  <IconButton label={`Tools of ${s.name}`} title="Connect, list its tools, choose which keys may call and their prices" onClick={() => setTools(s)}>
                    <TryIcon />
                  </IconButton>
                  <MenuButton
                    label={`More for ${s.name}`}
                    className="icon-btn"
                    items={[
                      { label: s.enabled ? "Disable" : "Enable", onClick: () => update(s, { enabled: !s.enabled }).catch((e) => setError((e as Error).message)) },
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
                <td colSpan={9} className="empty">
                  No MCP servers yet.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
      {tools && (
        <ServerTools
          server={tools}
          onClose={() => setTools(null)}
          onSave={async (changes) => {
            await update(tools, changes);
            setTools(null);
          }}
        />
      )}
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
  const [auth, setAuth] = useState<AuthState>(authState(from));
  const [tags, setTags] = useState<string[]>(from?.tags ?? []);
  const [projectIds, setProjectIds] = useState<string[]>(from?.project_ids ?? []);
  const [timeout, setTimeoutSecs] = useState(String(from?.timeout_secs ?? 300));
  const [price, setPrice] = useState(String(from?.price_per_call ?? 0));
  const [enabled, setEnabled] = useState(from?.enabled ?? true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // A stored secret stays unless a new one is typed; a duplicate has none.
  const keepsSecret = !!initial?.has_secret;
  const timeoutSecs = Number(timeout);
  const badTimeout = !Number.isInteger(timeoutSecs) || timeoutSecs < 1 || timeoutSecs > 3600;
  const pricePerCall = price.trim() ? Number(price) : 0;
  const badPrice = !Number.isFinite(pricePerCall) || pricePerCall < 0;
  const missing =
    !name.trim() || !url.trim() || badTimeout || badPrice || authMissing(auth, keepsSecret);

  const submit = async () => {
    setBusy(true);
    try {
      await onSave({
        name: name.trim(),
        description: description.trim() || undefined,
        url: url.trim(),
        ...authInput(await withClient("mcp", url, auth)),
        tags,
        project_ids: projectIds,
        timeout_secs: timeoutSecs,
        enabled,
        // Set in the tools dialog; a duplicate starts with the same.
        allowed_tools: from?.allowed_tools ?? [],
        denied_tools: from?.denied_tools ?? [],
        price_per_call: pricePerCall,
        tool_prices: from?.tool_prices ?? {},
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
          hint="Clients call the server at /mcp/ followed by this name, and find its tools at /mcp as name__tool. Lowercase letters, digits, - and _. Renaming it changes both."
          mono
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="e.g. github"
          required
          autoFocus
        />
        <TextInput caption="Description" value={description} onChange={(e) => setDescription(e.target.value)} placeholder="optional" />
        <AuthFields
          savedSignInPer={initial?.auth === "oauth_user" ? signInPer(initial.oauth?.sign_in_by) : undefined}
          kind="mcp"
          url={url}
          urlField={(discover) => (
            <TextInput
              caption="URL"
              hint="The server's Streamable HTTP endpoint, often ending in /mcp. The gateway calls it from its own network: addresses on private networks need GATEWAY_UPSTREAM_ALLOWED_NETWORKS."
              mono
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder="https://mcp.example.com/mcp"
              required
              action={discover}
            />
          )}
          value={auth}
          onChange={setAuth}
          keepsSecret={keepsSecret}
          auto={!from}
        />
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
            caption="Price per call"
            hint="USD charged to the key for each tool call, once the tool has answered. Counts toward key and project budgets. Tools can have prices of their own in the tools dialog."
            inputMode="decimal"
            value={price}
            onChange={(e) => setPrice(e.target.value)}
          />
          <TextInput
            caption="Timeout"
            hint="Seconds a call may take, its streamed answer included: 1 to 3600."
            inputMode="numeric"
            value={timeout}
            onChange={(e) => setTimeoutSecs(e.target.value)}
          />
        </div>
        <Toggle caption="Enabled" hint="A disabled server answers 404, as if it did not exist, and its tools leave /mcp." checked={enabled} onChange={setEnabled} />
      </div>
    </Modal>
  );
}

/**
 * Connects to the server through the admin API and lists its tools, with a switch for whether keys
 * may call each one and its price; and how to connect a client.
 */
function ServerTools({ server, onSave, onClose }: { server: McpServer; onSave: (changes: Partial<McpServerInput>) => Promise<void>; onClose: () => void }) {
  const [result, setResult] = useState<McpServerTestResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // "listed": keys call only the tools switched on, new ones not; "all": every tool but those switched off.
  const [mode, setMode] = useState<"all" | "listed">(server.allowed_tools.length ? "listed" : "all");
  const [on, setOn] = useState<Record<string, boolean>>({});
  const [prices, setPrices] = useState<Record<string, string>>(Object.fromEntries(Object.entries(server.tool_prices).map(([t, p]) => [t, String(p)])));
  const [dirty, setDirty] = useState(false);

  const run = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api<McpServerTestResult>(`/mcp-servers/${server.id}/test`, { method: "POST" });
      setResult(r);
      setOn(
        Object.fromEntries(
          r.tools.map((t) => [t.name, server.allowed_tools.length ? server.allowed_tools.includes(t.name) : !server.denied_tools.includes(t.name)]),
        ),
      );
    } catch (e) {
      setResult(null);
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }, [server]);

  useEffect(() => {
    run();
  }, [run]);

  const offered = result?.tools.map((t) => t.name) ?? [];
  const priceOf = (t: string) => (prices[t]?.trim() ? Number(prices[t]) : null);
  const badPrice = Object.keys(prices).some((t) => {
    const p = priceOf(t);
    return p != null && (!Number.isFinite(p) || p < 0);
  });

  const save = async () => {
    setBusy(true);
    try {
      // Tools the server no longer offers keep what was set for them.
      const gone = (list: string[]) => list.filter((t) => !offered.includes(t));
      const switched = (value: boolean) => offered.filter((t) => on[t] === value);
      await onSave({
        allowed_tools: mode === "listed" ? [...switched(true), ...gone(server.allowed_tools)] : [],
        denied_tools: mode === "all" ? [...switched(false), ...gone(server.denied_tools)] : [],
        tool_prices: Object.fromEntries(
          Object.keys(prices)
            .map((t) => [t, priceOf(t)] as const)
            .filter((e): e is readonly [string, number] => e[1] != null),
        ),
      });
    } catch (e) {
      setError((e as Error).message);
      setBusy(false);
    }
  };

  const change = <T,>(set: (v: T) => void) => (v: T) => {
    set(v);
    setDirty(true);
  };
  const config = JSON.stringify(
    // biome-ignore lint/suspicious/noTemplateCurlyInString: the placeholder clients replace with a key.
    { mcpServers: { [server.name]: { type: "http", url: endpoint(server.name), headers: { Authorization: "Bearer ${GATEWAY_API_KEY}" } } } },
    null,
    2,
  );
  const info = result?.server_info;
  return (
    <Modal
      title={`Tools of ${server.name}`}
      size="wide"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{dirty ? "Cancel" : "Close"}</Button>
          <Button onClick={run} disabled={busy}>
            {busy && !result ? "Connecting…" : "Connect again"}
          </Button>
          <Button variant="primary" onClick={save} disabled={busy || !dirty || badPrice || !result?.ok}>
            Save
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
          <div className="form">
            <Segmented
              caption="Keys may call"
              hint="Every tool: also tools the server adds later, but not those switched off. Only the tools switched on: tools the server adds later stay hidden until you switch them on. Hidden tools are left out of the tool lists and refused."
              options={[
                { value: "all", label: "Every tool" },
                { value: "listed", label: "Only the tools switched on" },
              ]}
              value={mode}
              onChange={change(setMode)}
            />
          </div>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>On</th>
                  <th>Tool</th>
                  <th>Description</th>
                  <th>Price per call</th>
                </tr>
              </thead>
              <tbody>
                {result.tools.map((t) => (
                  <tr key={t.name}>
                    <td>
                      <Switch label={`Keys may call ${t.name}`} checked={!!on[t.name]} onChange={(v) => change(setOn)({ ...on, [t.name]: v })} />
                    </td>
                    <td className="mono">{t.name}</td>
                    <td className="muted">{t.description}</td>
                    <td>
                      <TextInput
                        caption="USD"
                        mono
                        inputMode="decimal"
                        aria-label={`Price per call of ${t.name}`}
                        value={prices[t.name] ?? ""}
                        placeholder={String(server.price_per_call)}
                        onChange={(e) => change(setPrices)({ ...prices, [t.name]: e.target.value })}
                      />
                    </td>
                  </tr>
                ))}
                {result.tools.length === 0 && (
                  <tr>
                    <td colSpan={4} className="empty">
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
        {/* biome-ignore lint/suspicious/noTemplateCurlyInString: the placeholder clients replace with a key. */}
        <span className="mono">{"${GATEWAY_API_KEY}"}</span> with a virtual key. With <span className="mono">{endpoint()}</span> as the URL instead, the client gets the tools of every server the key may use
        {!server.enabled && "; this server is disabled until you enable it"}.
      </p>
      <pre className="raw">{config}</pre>
    </Modal>
  );
}
