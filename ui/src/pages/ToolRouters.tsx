import { useCallback, useEffect, useState } from "react";
import { api, type Deployment, type McpServer, type McpServerTestResult, type ToolRoute, type ToolRouter, type ToolRouterInput } from "../api";
import { Banner, Button, MenuButton, MultiSelect, PageActions, Select, TextArea, TextInput, Toggle } from "../components/controls";
import { Badge, ConfirmDialog, EditIcon, ErrorBanner, IconButton, MenuIcon, Modal, TrashIcon, type ConfirmRequest } from "../components/ui";
import { dateTime } from "../format";

/** A router as the admin API takes it back. */
const toInput = (r: ToolRouter): ToolRouterInput => ({
  name: r.name,
  description: r.description ?? null,
  decision_model: r.decision_model,
  instructions: r.instructions ?? null,
  routes: r.routes,
  threshold: r.threshold,
  max_routes: r.max_routes ?? null,
  enabled: r.enabled,
});

const DEFAULT_QUESTION = "Will answering the conversation's latest request need these tools? Answer true only if the answer would use them.";

/** Tool routers: a name requests use like an MCP server's, whose decision model picks which routes'
 *  tools each request gets. */
export function ToolRouters() {
  const [rows, setRows] = useState<ToolRouter[]>([]);
  const [servers, setServers] = useState<McpServer[]>([]);
  const [decisionModels, setDecisionModels] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<ToolRouter | "new" | null>(null);
  const [copyOf, setCopyOf] = useState<ToolRouter | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);

  const load = useCallback(async () => {
    try {
      const [r, s, d] = await Promise.all([api<ToolRouter[]>("/tool-routers"), api<McpServer[]>("/mcp-servers"), api<Deployment[]>("/deployments")]);
      setRows(r);
      setServers(s);
      setDecisionModels([...new Set(d.filter((x) => x.mode === "decision").map((x) => x.model_name))].sort());
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const serverName = (id: string) => servers.find((s) => s.id === id)?.name ?? "missing server";

  const toggle = async (r: ToolRouter) => {
    try {
      await api(`/tool-routers/${r.id}`, { method: "PUT", json: { ...toInput(r), enabled: !r.enabled } });
      await load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const remove = (r: ToolRouter) =>
    setConfirming({
      title: "Delete tool router",
      message: (
        <>
          Delete tool router <strong>{r.name}</strong>? Requests that name it are refused. Its MCP servers are not changed.
        </>
      ),
      action: async () => {
        await api(`/tool-routers/${r.id}`, { method: "DELETE" });
        await load();
      },
    });

  return (
    <div className="page">
      <div className="page-head">
        <p className="muted small">
          A model request names a router like an MCP server, as <span className="mono">{`{"type": "mcp", "server_label": "{name}"}`}</span>. For each request,
          its decision model picks which routes' tools the model gets.
        </p>
        <PageActions actions={[{ label: "New tool router", onClick: () => setEditing("new"), primary: true }]} />
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Decision model</th>
              <th>Routes</th>
              <th>Status</th>
              <th>Updated</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.id}>
                <td>
                  <span className="mono">{r.name}</span>
                  {r.description && <div className="muted small">{r.description}</div>}
                </td>
                <td className="mono">
                  {r.decision_model}
                  {!decisionModels.includes(r.decision_model) && (
                    <>
                      {" "}
                      <Badge tone="critical">missing</Badge>
                    </>
                  )}
                </td>
                <td>
                  {r.routes.map((x) => (
                    <div key={x.name} title={x.description}>
                      {x.name} <span className="muted small mono">{serverName(x.server_id)}{x.tools?.length ? `: ${x.tools.join(", ")}` : ""}</span>
                    </div>
                  ))}
                </td>
                <td>{r.enabled ? <Badge tone="good">Enabled</Badge> : <Badge tone="neutral">Disabled</Badge>}</td>
                <td className="when">{dateTime(r.updated_at)}</td>
                <td className="actions">
                  <IconButton label={`Edit ${r.name}`} title="Edit" onClick={() => setEditing(r)}>
                    <EditIcon />
                  </IconButton>
                  <MenuButton
                    label={`More for ${r.name}`}
                    className="icon-btn"
                    items={[
                      { label: r.enabled ? "Disable" : "Enable", onClick: () => toggle(r) },
                      {
                        label: "Duplicate",
                        onClick: () => {
                          setCopyOf(r);
                          setEditing("new");
                        },
                      },
                      { label: "Delete", danger: true, onClick: () => remove(r) },
                    ]}
                  >
                    <MenuIcon />
                  </MenuButton>
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={6} className="empty">
                  No tool routers yet.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
      {editing && (
        <ToolRouterForm
          initial={editing === "new" ? undefined : editing}
          copyOf={editing === "new" ? (copyOf ?? undefined) : undefined}
          servers={servers}
          decisionModels={decisionModels}
          onClose={() => {
            setEditing(null);
            setCopyOf(null);
          }}
          onDelete={
            editing === "new"
              ? undefined
              : () => {
                  setEditing(null);
                  remove(editing);
                }
          }
          onSave={async (input) => {
            if (editing === "new") await api("/tool-routers", { method: "POST", json: input });
            else await api(`/tool-routers/${editing.id}`, { method: "PUT", json: input });
            setEditing(null);
            setCopyOf(null);
            await load();
          }}
        />
      )}
    </div>
  );
}

const NAME = /^[a-z0-9][a-z0-9_-]{0,63}$/;
const ROUTE_NAME = /^[A-Za-z0-9_-]{1,64}$/;

function ToolRouterForm({
  initial,
  copyOf,
  servers,
  decisionModels,
  onSave,
  onDelete,
  onClose,
}: {
  initial?: ToolRouter;
  copyOf?: ToolRouter;
  servers: McpServer[];
  decisionModels: string[];
  onSave: (r: ToolRouterInput) => Promise<void>;
  onDelete?: () => void;
  onClose: () => void;
}) {
  const from = initial ?? copyOf;
  const [name, setName] = useState(initial?.name ?? (copyOf ? `${copyOf.name}-copy` : ""));
  const [description, setDescription] = useState(from?.description ?? "");
  const [decisionModel, setDecisionModel] = useState(from?.decision_model ?? decisionModels[0] ?? "");
  const [instructions, setInstructions] = useState(from?.instructions ?? "");
  const [routes, setRoutes] = useState<ToolRoute[]>(from?.routes ?? [{ name: "", description: "", server_id: servers[0]?.id ?? "", tools: [] }]);
  const [threshold, setThreshold] = useState(String(from?.threshold ?? 0.5));
  const [maxRoutes, setMaxRoutes] = useState(from?.max_routes ? String(from.max_routes) : "");
  const [enabled, setEnabled] = useState(from?.enabled ?? true);
  /** Each server's tools, listed when a route first names it. */
  const [tools, setTools] = useState<Record<string, string[] | "failed">>({});
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    for (const id of new Set(routes.map((r) => r.server_id).filter(Boolean))) {
      if (tools[id] !== undefined) continue;
      setTools((t) => ({ ...t, [id]: [] }));
      api<McpServerTestResult>(`/mcp-servers/${id}/test`, { method: "POST" })
        .then((r) => setTools((t) => ({ ...t, [id]: r.ok ? r.tools.map((x) => x.name) : "failed" })))
        .catch(() => setTools((t) => ({ ...t, [id]: "failed" })));
    }
  }, [routes.map((r) => r.server_id).join()]);

  const patch = (i: number, p: Partial<ToolRoute>) => setRoutes((rs) => rs.map((r, j) => (j === i ? { ...r, ...p } : r)));
  const names = routes.map((r) => r.name.trim());
  const duplicateName = new Set(names).size !== names.length;
  const t = Number(threshold);
  const badThreshold = !(t > 0 && t < 1);
  const m = maxRoutes.trim() ? Number(maxRoutes) : null;
  const badMax = m !== null && !(Number.isInteger(m) && m >= 1);
  const invalid =
    !NAME.test(name.trim()) ||
    name.includes("__") ||
    !decisionModel ||
    !routes.length ||
    duplicateName ||
    badThreshold ||
    badMax ||
    routes.some((r) => !ROUTE_NAME.test(r.name.trim()) || !r.description.trim() || !r.server_id);

  const submit = async () => {
    setBusy(true);
    try {
      await onSave({
        name: name.trim(),
        description: description.trim() || null,
        decision_model: decisionModel,
        instructions: instructions.trim() || null,
        routes: routes.map((r) => ({ name: r.name.trim(), description: r.description.trim(), server_id: r.server_id, tools: r.tools })),
        threshold: t,
        max_routes: m,
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
      title={initial ? "Edit tool router" : "New tool router"}
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
          <Button variant="primary" disabled={busy || invalid} onClick={submit}>
            {initial ? "Save" : "Add tool router"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {(!decisionModels.length || !servers.length) && (
        <Banner tone="warning">
          A tool router needs a decision model and MCP servers. Add a <a href="#/models/deployments">decision model deployment</a> and MCP servers first.
        </Banner>
      )}
      <div className="form">
        <div className="row with-switch">
          <TextInput
            caption="Name"
            hint="What requests name it by, like an MCP server: lowercase letters, digits, - and _. No MCP server may have the same name."
            mono
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. support"
            required
            autoFocus
          />
          <Select
            caption="Decision model"
            hint="Decides, for each route, whether a request needs its tools: one decision call per request."
            value={decisionModel}
            required
            onChange={setDecisionModel}
            empty="No decision models"
            options={[
              ...(decisionModel && !decisionModels.includes(decisionModel) ? [{ value: decisionModel, label: `${decisionModel} (missing)` }] : []),
              ...decisionModels.map((d) => ({ value: d, label: d })),
            ]}
          />
          <Toggle caption="Active" checked={enabled} onChange={setEnabled} />
        </div>
        <TextInput caption="Description (optional)" value={description} onChange={(e) => setDescription(e.target.value)} />
        <TextArea
          caption="Question (optional)"
          hint={`Asked for every route, with the route's description as what “true” means. Leave empty for: “${DEFAULT_QUESTION}”`}
          rows={2}
          value={instructions}
          onChange={(e) => setInstructions(e.target.value)}
        />

        <fieldset>
          <legend>Routes</legend>
          <div className="routes-editor">
            {routes.map((r, i) => {
              const listed = tools[r.server_id];
              const known = Array.isArray(listed) ? listed : [];
              const options = [...(r.tools ?? []).filter((x) => !known.includes(x)), ...known].map((x) => ({ value: x, label: x }));
              return (
                <div key={i} className="route-row">
                  <TextInput
                    caption="Name"
                    hint="Shown in the response's x-gateway-tool-routes header and the playground. Unique within the router."
                    value={r.name}
                    onChange={(e) => patch(i, { name: e.target.value })}
                    aria-label={`Route ${i + 1} name`}
                    placeholder="e.g. billing"
                    required
                  />
                  <Select
                    caption="MCP server"
                    label={`Route ${i + 1} MCP server`}
                    value={r.server_id}
                    required
                    onChange={(v) => patch(i, { server_id: v, tools: [] })}
                    empty="No MCP servers"
                    options={servers.map((s) => ({ value: s.id, label: s.name, note: s.enabled ? undefined : "disabled" }))}
                  />
                  <IconButton label={`Remove route ${i + 1}`} title="Remove" onClick={() => setRoutes((rs) => rs.filter((_, j) => j !== i))}>
                    <TrashIcon />
                  </IconButton>
                  <MultiSelect
                    className="route-wide"
                    caption="Tools"
                    label={`Route ${i + 1} tools`}
                    hint="The server's tools this route offers, after the server's own filters. All offers every tool the server lists."
                    none="All"
                    values={r.tools ?? []}
                    onChange={(v) => patch(i, { tools: v })}
                    empty={listed === "failed" ? "The server's tools could not be listed" : "Listing the server's tools…"}
                    options={options}
                  />
                  <TextArea
                    className="route-wide"
                    caption="When it is needed"
                    rows={2}
                    value={r.description}
                    onChange={(e) => patch(i, { description: e.target.value })}
                    placeholder="e.g. “Questions about invoices, payments and refunds.”"
                    aria-label={`Route ${i + 1} description`}
                    required
                  />
                </div>
              );
            })}
          </div>
          {duplicateName && <p className="small" role="alert">Each route needs its own name.</p>}
          <Button
            variant="ghost"
            disabled={!servers.length || routes.length >= 64}
            onClick={() => setRoutes((rs) => [...rs, { name: "", description: "", server_id: servers[0]?.id ?? "", tools: [] }])}
          >
            Add route
          </Button>
        </fieldset>

        <div className="row">
          <TextInput
            caption="Threshold"
            hint="A route is offered when the decision model is at least this sure that the request needs it: above 0 and below 1. When no route reaches it, the model gets no tools of this router. When the decision fails, it gets every route's."
            value={threshold}
            onChange={(e) => setThreshold(e.target.value)}
            inputMode="decimal"
            aria-invalid={badThreshold || undefined}
          />
          <TextInput
            caption="At most (optional)"
            hint="At most this many routes per request, the surest first. Empty for no limit."
            value={maxRoutes}
            onChange={(e) => setMaxRoutes(e.target.value)}
            inputMode="numeric"
            placeholder="No limit"
            aria-invalid={badMax || undefined}
          />
        </div>
      </div>
    </Modal>
  );
}
