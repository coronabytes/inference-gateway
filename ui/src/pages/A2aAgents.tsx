import { useCallback, useEffect, useState } from "react";
import { api, type A2aAgent, type A2aAgentInput, type A2aAgentTestResult, type NamedItem, type TagInfo } from "../api";
import { Button, MenuButton, MultiSelect, PageActions, TextInput, Toggle } from "../components/controls";
import { TagEditor, TagList } from "../components/Tags";
import { Badge, ConfirmDialog, EditIcon, ErrorBanner, IconButton, MenuIcon, Modal, TryIcon, type ConfirmRequest } from "../components/ui";
import { dateTime, ms, usd } from "../format";
import { useAdminData } from "../useAdminData";
import { AUTH_LABEL, AuthFields, type AuthState, authInput, authMissing, authState, signInPer, withClient } from "./UpstreamAuth";

/** Where clients find an agent's card through the gateway. */
const cardUrl = (name: string) => `${location.origin}/a2a/${name}/.well-known/agent-card.json`;

/** An agent as the admin API takes it back, keeping its stored secret. */
const toInput = (a: A2aAgent): A2aAgentInput => ({
  name: a.name,
  description: a.description,
  card_url: a.card_url,
  auth: a.auth,
  auth_header: a.auth_header,
  oauth: a.oauth,
  tags: a.tags,
  project_ids: a.project_ids,
  timeout_secs: a.timeout_secs,
  price_per_message: a.price_per_message,
  enabled: a.enabled,
});

/** A2A agents: agents the gateway proxies at `/a2a/{name}`, with its own credentials for them. */
export function A2aAgents() {
  const { data, error, setError, reload: load } = useAdminData<[A2aAgent[], NamedItem[], TagInfo[]]>(["/a2a-agents", "/projects/options", "/tags"]);
  const [rows, projects, tagInfo] = data ?? [[], [], []];
  const tags = tagInfo.map((t) => t.tag);
  const [editing, setEditing] = useState<A2aAgent | "new" | null>(null);
  const [copyOf, setCopyOf] = useState<A2aAgent | null>(null);
  const [trying, setTrying] = useState<A2aAgent | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);

  const toggle = async (a: A2aAgent) => {
    try {
      await api(`/a2a-agents/${a.id}`, { method: "PUT", json: { ...toInput(a), enabled: !a.enabled } });
      await load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const remove = (a: A2aAgent) =>
    setConfirming({
      title: "Delete A2A agent",
      message: (
        <>
          Delete A2A agent <strong>{a.name}</strong>? Clients using <span className="mono">/a2a/{a.name}</span> stop working, and its tasks can no longer be
          followed through the gateway. The agent itself is not changed.
        </>
      ),
      action: async () => {
        await api(`/a2a-agents/${a.id}`, { method: "DELETE" });
        await load();
      },
    });

  const projectNames = (ids: string[]) => ids.map((id) => projects.find((p) => p.id === id)?.name ?? "unknown project");

  return (
    <div className="page">
      <div className="page-head">
        <p className="muted small">
          Clients find an agent at <span className="mono">{cardUrl("{name}")}</span>, an agent card with the gateway's addresses. They authenticate with a
          virtual key.
        </p>
        <PageActions actions={[{ label: "New A2A agent", onClick: () => setEditing("new"), primary: true }]} />
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Agent card</th>
              <th>Authentication</th>
              <th>Price</th>
              <th>Access</th>
              <th>Status</th>
              <th>Updated</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((a) => (
              <tr key={a.id}>
                <td>
                  <span className="mono">{a.name}</span>
                  {a.description && <div className="muted small">{a.description}</div>}
                </td>
                <td className="mono clip" title={a.card_url}>
                  {a.card_url.replace(/^https?:\/\//, "")}
                </td>
                <td>
                  {AUTH_LABEL[a.auth]}
                  {a.auth === "header" && <span className="muted small mono"> {a.auth_header}</span>}
                </td>
                <td>{usd(a.price_per_message)}</td>
                <td>
                  <TagList tags={a.tags} projects={projectNames(a.project_ids)} empty="Every key" />
                </td>
                <td>{a.enabled ? <Badge tone="good">Enabled</Badge> : <Badge tone="neutral">Disabled</Badge>}</td>
                <td className="when">{dateTime(a.updated_at)}</td>
                <td className="actions">
                  <IconButton label={`Edit ${a.name}`} title="Edit" onClick={() => setEditing(a)}>
                    <EditIcon />
                  </IconButton>
                  <IconButton label={`Try ${a.name}`} title="Fetch its card" onClick={() => setTrying(a)}>
                    <TryIcon />
                  </IconButton>
                  <MenuButton
                    label={`More for ${a.name}`}
                    className="icon-btn"
                    items={[
                      { label: a.enabled ? "Disable" : "Enable", onClick: () => toggle(a) },
                      {
                        label: "Duplicate",
                        onClick: () => {
                          setCopyOf(a);
                          setEditing("new");
                        },
                      },
                      { label: "Delete", danger: true, onClick: () => remove(a) },
                    ]}
                  >
                    <MenuIcon />
                  </MenuButton>
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={8} className="empty">
                  No A2A agents yet.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
      {trying && <TryAgent agent={trying} onClose={() => setTrying(null)} />}
      {editing && (
        <AgentForm
          initial={editing === "new" ? undefined : editing}
          copyOf={editing === "new" ? (copyOf ?? undefined) : undefined}
          projects={projects}
          knownTags={tags}
          onClose={() => {
            setEditing(null);
            setCopyOf(null);
          }}
          onSave={async (input) => {
            if (editing === "new") await api("/a2a-agents", { method: "POST", json: input });
            else await api(`/a2a-agents/${editing.id}`, { method: "PUT", json: input });
            setEditing(null);
            setCopyOf(null);
            await load();
          }}
        />
      )}
    </div>
  );
}

function AgentForm({
  initial,
  copyOf,
  projects,
  knownTags,
  onSave,
  onClose,
}: {
  initial?: A2aAgent;
  /** Duplicate: a new agent starting from this one, without its secret. */
  copyOf?: A2aAgent;
  projects: NamedItem[];
  knownTags: string[];
  onSave: (a: A2aAgentInput) => Promise<void>;
  onClose: () => void;
}) {
  const from = initial ?? copyOf;
  const [name, setName] = useState(initial?.name ?? (copyOf ? `${copyOf.name}-copy` : ""));
  const [description, setDescription] = useState(from?.description ?? "");
  const [url, setUrl] = useState(from?.card_url ?? "");
  const [auth, setAuth] = useState<AuthState>(authState(from));
  const [tags, setTags] = useState<string[]>(from?.tags ?? []);
  const [projectIds, setProjectIds] = useState<string[]>(from?.project_ids ?? []);
  const [timeout, setTimeoutSecs] = useState(String(from?.timeout_secs ?? 600));
  const [price, setPrice] = useState(String(from?.price_per_message ?? 0));
  const [enabled, setEnabled] = useState(from?.enabled ?? true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // A stored secret stays unless a new one is typed; a duplicate has none.
  const keepsSecret = !!initial?.has_secret;
  const timeoutSecs = Number(timeout);
  const badTimeout = !Number.isInteger(timeoutSecs) || timeoutSecs < 1 || timeoutSecs > 3600;
  const pricePerMessage = price.trim() ? Number(price) : 0;
  const badPrice = !Number.isFinite(pricePerMessage) || pricePerMessage < 0;
  const missing =
    !name.trim() || !url.trim() || badTimeout || badPrice || authMissing(auth, keepsSecret);

  const submit = async () => {
    setBusy(true);
    try {
      await onSave({
        name: name.trim(),
        description: description.trim() || undefined,
        card_url: url.trim(),
        ...authInput(await withClient("a2a", url, auth)),
        tags,
        project_ids: projectIds,
        timeout_secs: timeoutSecs,
        price_per_message: pricePerMessage,
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
      title={initial ? "Edit A2A agent" : "New A2A agent"}
      onClose={onClose}
      size="medium"
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy || missing} onClick={submit}>
            {initial ? "Save" : "Create A2A agent"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="form">
        <TextInput
          caption="Name"
          hint="Clients reach the agent at /a2a/ followed by this name. Lowercase letters, digits, - and _. Renaming it changes the address clients use."
          mono
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="e.g. travel-planner"
          required
          autoFocus
        />
        <TextInput caption="Description" value={description} onChange={(e) => setDescription(e.target.value)} placeholder="optional" />
        <AuthFields
          savedSignInPer={initial?.auth === "oauth_user" ? signInPer(initial.oauth?.sign_in_by) : undefined}
          kind="a2a"
          url={url}
          urlField={(discover) => (
            <TextInput
              caption="Agent card"
              hint="The address of the agent's card. The agent's address alone gets /.well-known/agent-card.json. The card says where the agent's interfaces are; the gateway proxies the newest JSON-RPC and HTTP+JSON ones, in A2A 1.0 or 0.3."
              mono
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder="https://agent.example.com/.well-known/agent-card.json"
              required
              action={discover}
            />
          )}
          value={auth}
          onChange={setAuth}
          keepsSecret={keepsSecret}
        />
        <TagEditor
          caption="Tags"
          hint={
            <ul className="hint-list">
              <li>
                <strong>Keys with tags</strong> can use this agent only if they hold every one of its tags.
              </li>
              <li>
                <strong>Keys without tags</strong> can use every agent not limited to other projects.
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
                <strong>With projects</strong>, only keys in one of them can use this agent. Their tags still apply.
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
            caption="Price per message"
            hint="USD charged to the key for each message sent to the agent, once it answered. Counts toward key and project budgets."
            inputMode="decimal"
            value={price}
            onChange={(e) => setPrice(e.target.value)}
          />
          <TextInput
            caption="Timeout"
            hint="Seconds an answer that is not streamed may take: 1 to 3600. Streams follow their task as long as it runs."
            inputMode="numeric"
            value={timeout}
            onChange={(e) => setTimeoutSecs(e.target.value)}
          />
        </div>
        <Toggle caption="Enabled" hint="A disabled agent answers 404, as if it did not exist." checked={enabled} onChange={setEnabled} />
      </div>
    </Modal>
  );
}

/** Fetches the agent's card through the admin API: what the gateway proxies of it, and its skills. */
function TryAgent({ agent, onClose }: { agent: A2aAgent; onClose: () => void }) {
  const [result, setResult] = useState<A2aAgentTestResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setResult(await api<A2aAgentTestResult>(`/a2a-agents/${agent.id}/test`, { method: "POST" }));
    } catch (e) {
      setResult(null);
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }, [agent.id]);

  useEffect(() => {
    run();
  }, [run]);

  return (
    <Modal
      title={`Try ${agent.name}`}
      size="wide"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Close</Button>
          <Button variant="primary" onClick={run} disabled={busy}>
            {busy ? "Fetching…" : "Fetch again"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {result && !result.ok && <ErrorBanner error={`The card could not be used: ${result.error ?? "no details"}`} onDismiss={() => setResult(null)} />}
      {result?.ok && (
        <div className="guardrail-result">
          <div className="guardrail-result-head">
            <Badge tone="good">Card found</Badge>
            <span className="muted small">{[result.name && `${result.name}${result.version ? ` ${result.version}` : ""}`, ms(result.latency_ms)].filter(Boolean).join(" · ")}</span>
          </div>
          {result.description && <p className="muted">{result.description}</p>}
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Interface</th>
                  <th>A2A</th>
                  <th>At the agent</th>
                  <th>Through the gateway</th>
                </tr>
              </thead>
              <tbody>
                {result.interfaces?.map((i) => (
                  <tr key={i.binding}>
                    <td>{i.binding === "JSONRPC" ? "JSON-RPC" : "HTTP+JSON"}</td>
                    <td>{i.version}</td>
                    <td className="mono clip" title={i.url}>
                      {i.url}
                    </td>
                    <td className="mono">{`${location.origin}/a2a/${agent.name}${i.binding === "JSONRPC" ? "" : "/rest"}`}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Skill</th>
                  <th>Description</th>
                </tr>
              </thead>
              <tbody>
                {result.skills?.map((s) => (
                  <tr key={s.name}>
                    <td>{s.name}</td>
                    <td className="muted">{s.description}</td>
                  </tr>
                ))}
                {!result.skills?.length && (
                  <tr>
                    <td colSpan={2} className="empty">
                      The card lists no skills.
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
        A2A clients start from the agent card: give them <span className="mono">{cardUrl(agent.name)}</span>, with a virtual key as the bearer token for the card
        and for the agent{!agent.enabled && "; the agent is disabled until you enable it"}.
      </p>
    </Modal>
  );
}
