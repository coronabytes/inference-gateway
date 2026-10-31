import { Fragment, useCallback, useEffect, useMemo, useState } from "react";
import { api, type AuditEntry, type LogRow } from "../api";
import { Button, MultiSelect, Select, Tabs, TextInput, TimeRange, useHashTab, useTimeRange } from "../components/controls";
import { Bodies } from "../components/Transcript";
import { KeyFilter, setKeyFilter } from "../components/KeyFilter";
import { LabelFilter } from "../components/LabelInput";
import { Badge, ChildArrowIcon, ErrorBanner, SidePanel } from "../components/ui";
import { compact, dateTime, ms, relative, usd } from "../format";
import { labelQuery, withLabel, type Labels } from "../labels";

const RANGES = ["1h", "24h", "7d", "30d", "month", "last_month", "custom"] as const;

/** Toggle `id` in a set of open entries, for a state setter. */
const flip = (id: string) => (s: Set<string>) => {
  const next = new Set(s);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  return next;
};

/** An error with the JSON body an upstream returned, if any, indented. */
function readableError(error: string): string {
  const at = error.indexOf("{");
  if (at < 0) return error;
  try {
    return error.slice(0, at) + JSON.stringify(JSON.parse(error.slice(at)), null, 2);
  } catch {
    return error;
  }
}

/** The tools a logged request's response called; clicking one filters the log by it. */
function ToolList({ tools, onPick }: { tools: string[] | null; onPick: (tool: string) => void }) {
  if (!tools?.length) return <span className="muted">–</span>;
  return (
    <span className="tags">
      {tools.map((t) => (
        <button key={t} type="button" className="tag label-tag" title={`${t}: show only requests that called this tool`} onClick={() => onPick(t)}>
          {t}
        </button>
      ))}
    </span>
  );
}

/** A request's labels; clicking one filters the log by it. */
function LabelList({ labels, onPick }: { labels: LogRow["labels"]; onPick: (key: string, value: string) => void }) {
  const entries = Object.entries(labels ?? {});
  if (entries.length === 0) return <span className="muted">–</span>;
  return (
    <span className="tags">
      {entries.map(([k, v]) => (
        <button key={k} type="button" className="tag label-tag" title={`${k}=${v}: show only requests with this label`} onClick={() => onPick(k, v)}>
          {k}={v}
        </button>
      ))}
    </span>
  );
}

/** What a logged entry's `bodies` says, for its details. */
const BODIES_STATUS: Record<NonNullable<LogRow["bodies"]>, string> = {
  stored: "stored",
  truncated: "stored, long texts cut to the size limit",
  expired: "expired: removed after their retention",
  dropped: "not kept: the gateway's buffer was full, or the database refused them",
  deleted: "deleted on request",
};

/** What the table leaves out: the whole error, ids, the token breakdown, the logged bodies. */
function RequestDetails({ r, routed }: { r: LogRow; routed: boolean }) {
  const readable = r.bodies === "stored" || r.bodies === "truncated";
  return (
    <>
      {r.error && <pre className="raw wrap">{readableError(r.error)}</pre>}
      <table className="detail-table">
        <tbody>
          <tr>
            <th>Time</th>
            <td>{new Date(r.ts).toLocaleString()}</td>
          </tr>
          <tr>
            <th>Request ID</th>
            <td className="mono">{r.id}</td>
          </tr>
          <tr>
            <th>API</th>
            <td>
              {r.api}
              {r.stream && <span className="muted"> · streamed</span>}
            </td>
          </tr>
          <tr>
            <th>Deployment</th>
            <td>
              {r.deployment_id == null ? (
                "–"
              ) : r.upstream_model == null ? (
                <span className="muted">deleted</span>
              ) : (
                <>
                  <span className="mono">{r.upstream_model}</span> on {r.provider ?? "–"} with <span className="mono">{r.credential_name ?? "–"}</span>
                </>
              )}
              {r.deployment_id && <div className="mono muted small">{r.deployment_id}</div>}
            </td>
          </tr>
          <tr>
            <th>Tokens</th>
            <td>
              {compact(r.input_tokens)} in, {compact(r.cache_read_tokens)} cache read, {compact(r.cache_write_tokens)} cache write, {compact(r.output_tokens)} out
            </td>
          </tr>
          {r.searches > 0 && (
            <tr>
              <th>{r.api === "rerank" ? "Searches" : "Web searches"}</th>
              <td>
                {r.searches} · {usd(r.search_cost_usd)} of the cost
              </td>
            </tr>
          )}
          <tr>
            <th>Attempts</th>
            <td>{r.attempts}</td>
          </tr>
          {r.guardrails && (
            <tr>
              <th>Guardrails</th>
              <td>
                {r.guardrails.action} by <span className="mono">{r.guardrails.applied.join(", ")}</span>
                {r.guardrails.findings.length > 0 && <> · found <span className="mono">{r.guardrails.findings.join(", ")}</span></>} ·{" "}
                {usd(r.guardrail_cost_usd)} of the cost
              </td>
            </tr>
          )}
          {r.labels && (
            <tr>
              <th>Labels</th>
              <td className="mono">
                {Object.entries(r.labels)
                  .map(([k, v]) => `${k}=${v}`)
                  .join(", ")}
              </td>
            </tr>
          )}
          {r.bodies && (
            <tr>
              <th>Prompt and response</th>
              <td>
                {BODIES_STATUS[r.bodies]}
                {routed && " · the routed call beneath it in the log has the response"}
              </td>
            </tr>
          )}
        </tbody>
      </table>
      {readable && <Bodies key={r.id} id={r.id} api={r.api} />}
    </>
  );
}

/** What the guardrails did, as the request log's Show filter offers it. */
const GUARDRAIL_ACTIONS = ["blocked", "masked", "flagged"];

/** The request log, a tab of the Cluster page. */
const TABS = ["requests", "audit"] as const;

/** The Logs page: the request log and the audit log, each a tab. */
export function Logs() {
  const [tab, setTab] = useHashTab("logs", TABS);
  return (
    <div className="page">
      <Tabs
        label="Logs"
        options={[
          { value: "requests", label: "Request log" },
          { value: "audit", label: "Audit log" },
        ]}
        value={tab}
        onChange={setTab}
      />
      {tab === "audit" ? <AuditLog /> : <RequestLog />}
    </div>
  );
}

export function RequestLog() {
  const [rows, setRows] = useState<LogRow[]>([]);
  const time = useTimeRange(RANGES, "1h");
  // The KeyFilter's choice.
  const [keyId, setKeyId] = useState("");
  const [labels, setLabels] = useState<Labels>({});
  const labelFilter = labelQuery(labels);
  // Which entries to show: errors, those with stored prompts, and what the guardrails did.
  const [show, setShow] = useState<string[]>([]);
  // Prompt logging: a search of the stored prompts (sent on Enter), or a tool their responses called.
  const [contentDraft, setContentDraft] = useState("");
  const [content, setContent] = useState("");
  const [tool, setTool] = useState("");
  const [tools, setTools] = useState<{ name: string; requests: number }[]>([]);
  const [error, setError] = useState<string | null>(null);
  // The entry whose details the side panel shows; kept while the log refreshes around it.
  const [selected, setSelected] = useState<LogRow | null>(null);

  // A routed request's calls (decision, routed call) are shown beneath the router's entry.
  const { top, children } = useMemo(() => {
    const ids = new Set(rows.map((r) => r.id));
    const children = new Map<string, LogRow[]>();
    for (const r of rows) {
      if (r.parent_id && ids.has(r.parent_id)) children.set(r.parent_id, [...(children.get(r.parent_id) ?? []), r]);
    }
    for (const list of children.values()) list.sort((a, b) => a.ts.localeCompare(b.ts));
    return { top: rows.filter((r) => !r.parent_id || !ids.has(r.parent_id)), children };
  }, [rows]);

  const load = useCallback(async () => {
    if (time.query === null) return;
    const q = new URLSearchParams(time.query);
    q.set("limit", "200");
    setKeyFilter(q, keyId);
    if (labelFilter) q.set("labels", labelFilter);
    if (show.includes("errors")) q.set("errors", "true");
    if (show.includes("prompts")) q.set("bodies", "true");
    const guardrail = show.filter((s) => GUARDRAIL_ACTIONS.includes(s));
    if (guardrail.length) q.set("guardrail", guardrail.join(","));
    if (content) q.set("q", content);
    if (tool) q.set("tool", tool);
    try {
      setRows(await api<LogRow[]>(`/logs?${q}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [time.query, keyId, labelFilter, show, content, tool]);

  // Tools offered by the tool filter: those of the latest logged responses.
  useEffect(() => {
    api<{ name: string; requests: number }[]>("/logs/tools")
      .then(setTools)
      .catch(() => setTools([]));
  }, []);

  const pickLabel = (key: string, value: string) => setLabels((l) => withLabel(l, key, value));

  useEffect(() => {
    load();
    const t = setInterval(load, 10_000);
    return () => clearInterval(t);
  }, [load]);

  return (
    <>
      <div className="filters">
        <TimeRange {...time.props} />
        <KeyFilter value={keyId} onChange={setKeyId} />
        <LabelFilter value={labels} onChange={setLabels} />
        <MultiSelect
          caption="Show"
          hint="Errors, requests whose prompt and response are stored, and what the guardrails did. The guardrail outcomes show requests with any of them; together with errors or stored prompts, only those that are both."
          values={show}
          onChange={setShow}
          searchable={false}
          options={[
            { value: "errors", label: "Errors" },
            { value: "prompts", label: "With prompts" },
            { value: "blocked", label: "Blocked by a guardrail" },
            { value: "masked", label: "Masked by a guardrail" },
            { value: "flagged", label: "Flagged by a guardrail" },
          ]}
        />
        <form
          onSubmit={(e) => {
            e.preventDefault();
            setContent(contentDraft.trim());
          }}
        >
          <TextInput
            caption="Content"
            type="search"
            placeholder="search, Enter"
            hint='Searches the prompts of logged requests as they were sent (before any guardrail masked them) and their responses. Whole words; "a phrase", or, -word and prefix:* work.'
            value={contentDraft}
            onChange={(e) => {
              setContentDraft(e.target.value);
              if (!e.target.value) setContent("");
            }}
          />
        </form>
        {tools.length > 0 && (
          <Select
            caption="Tool called"
            value={tool}
            onChange={setTool}
            options={[{ value: "", label: "Any" }, ...tools.map((t) => ({ value: t.name, label: t.name, note: `${t.requests}` }))]}
          />
        )}
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>When</th>
              <th>Status</th>
              <th>Model</th>
              <th>Key</th>
              <th>Labels</th>
              <th>Tools</th>
              <th>Provider</th>
              <th className="num">Latency</th>
              <th className="num">TTFT</th>
              <th className="num">In / out</th>
              <th aria-label="Guardrails and prompt logging" />
              <th className="num">Cost</th>
            </tr>
          </thead>
          <tbody>
            {top.map((p) => {
              const calls = children.get(p.id) ?? [];
              return (
                <Fragment key={p.id}>
                  {[p, ...calls].map((r) => (
                    <Fragment key={r.id}>
                      <tr
                        className={["clickable", r === p ? "" : "child-row", selected?.id === r.id ? "selected" : ""].join(" ").trim()}
                        tabIndex={0}
                        aria-selected={selected?.id === r.id}
                        // Clicking a row shows its details; a second click hides them. Its own
                        // buttons (a label to filter by) keep their action.
                        onClick={(e) => (e.target as HTMLElement).closest("button, a") || setSelected((s) => (s?.id === r.id ? null : r))}
                        onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && e.target === e.currentTarget && (e.preventDefault(), setSelected((s) => (s?.id === r.id ? null : r)))}
                      >
                        <td className="when" title={dateTime(r.ts)}>
                          {r === p ? relative(r.ts) : <ChildArrowIcon />}
                        </td>
                        <td>
                          <Badge tone={r.status_code < 400 ? "good" : r.status_code === 429 ? "warning" : "critical"}>{r.status_code}</Badge>
                          {r.attempts > 1 && <span className="muted small"> ×{r.attempts}</span>}
                        </td>
                        <td className="mono model">
                          {r.model_name}
                          {r === p && calls.length > 0 && <span className="muted small"> router</span>}
                          {r.parent_id && r === p && <span className="muted small"> via router</span>}
                        </td>
                        <td>{r.key_name ?? <span className="muted">{r.key_id ? "deleted" : "–"}</span>}</td>
                        <td>
                          {/* A routed request's calls carry the router's labels. */}
                          {r === p && <LabelList labels={r.labels} onPick={pickLabel} />}
                        </td>
                        <td>
                          <ToolList tools={r.tool ? [r.tool] : r.tool_calls} onPick={setTool} />
                        </td>
                        <td>{r.provider ?? (r.api === "mcp" ? "MCP" : "–")}</td>
                        <td className="num">{ms(r.latency_ms)}</td>
                        <td className="num">{ms(r.ttft_ms)}</td>
                        <td className="num">
                          {compact(r.input_tokens + r.cache_read_tokens + r.cache_write_tokens)} / {compact(r.output_tokens)}
                        </td>
                        {/* Marks in a column of their own, so they line up whatever the cost's width. */}
                        <td className="marks">
                          {r.guardrails && r.guardrails.action !== "passed" && (
                            <span className={`guardrail-mark ${r.guardrails.action}`} title={`Guardrails ${r.guardrails.action} the prompt; see details`}>
                              {r.guardrails.action}
                            </span>
                          )}
                          {(r.bodies === "stored" || r.bodies === "truncated") && (
                            <span className="guardrail-mark logged" title="Its prompt and response are stored; see details">
                              logged
                            </span>
                          )}
                        </td>
                        <td className="num">{usd(r.cost_usd)}</td>
                      </tr>
                    </Fragment>
                  ))}
                </Fragment>
              );
            })}
            {rows.length === 0 && (
              <tr>
                <td colSpan={12} className="empty">No requests match these filters.</td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      {selected && (
        <SidePanel title={`${selected.model_name} · ${dateTime(selected.ts)}`} onClose={() => setSelected(null)}>
          <RequestDetails r={selected} routed={children.has(selected.id)} />
        </SidePanel>
      )}
    </>
  );
}

const TARGETS: Record<string, string> = {
  credential: "Credential",
  deployment: "Deployment",
  router: "Router",
  guardrail: "Guardrail",
  model: "Model",
  key: "Key",
  project: "Project",
  configuration: "Configuration",
  price_catalog: "Price catalog",
  oidc_settings: "Single sign-on",
  settings: "Settings",
  admin: "Admin",
  session: "Session",
};

const ACTIONS: Record<string, string> = {
  create: "Created",
  update: "Changed",
  delete: "Deleted",
  regenerate: "Regenerated",
  reset_spend: "Reset spend",
  clear_cooldown: "Cleared cooldown",
  import: "Imported",
  sign_in: "Signed in",
  sign_out: "Signed out",
};

const ACTORS: Record<AuditEntry["actor_kind"], string> = {
  master_key: "master key",
  virtual_key: "virtual key",
  session: "single sign-on",
  dev: "dev login",
  system: "scheduled",
};

function actionTone(action: string): "good" | "warning" | "critical" | "neutral" {
  if (action === "create") return "good";
  if (action === "delete") return "critical";
  if (action === "regenerate" || action === "reset_spend") return "warning";
  return "neutral";
}

/** Details that are all `{ from, to }` pairs (an update, a regenerated key) as fields and their change. */
function fieldChanges(e: AuditEntry): [string, { from: unknown; to: unknown }][] | null {
  const entries = Object.entries(e.details);
  const pairs = entries.filter((x): x is [string, { from: unknown; to: unknown }] => !!x[1] && typeof x[1] === "object" && "from" in x[1] && "to" in x[1]);
  return pairs.length > 0 && pairs.length === entries.length ? pairs : null;
}

function value(v: unknown): string {
  if (v === null || v === undefined) return "–";
  if (Array.isArray(v)) return v.length ? v.map(value).join(", ") : "none";
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

/** One line for the table: the changed fields, otherwise nothing (the details are one click away). */
function summary(e: AuditEntry): string {
  return fieldChanges(e)?.map(([k]) => k).join(", ") ?? "";
}

/** The audit log, a tab of the Cluster page. */
export function AuditLog() {
  const [rows, setRows] = useState<AuditEntry[]>([]);
  const time = useTimeRange(RANGES, "1h");
  const [type, setType] = useState("");
  const [search, setSearch] = useState("");
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (time.query === null) return;
    const q = new URLSearchParams(time.query);
    q.set("limit", "500");
    if (type) q.set("target_type", type);
    if (search.trim()) q.set("q", search.trim());
    try {
      setRows(await api<AuditEntry[]>(`/audit?${q}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [time.query, type, search]);

  useEffect(() => {
    load();
    const t = setInterval(load, 30_000);
    return () => clearInterval(t);
  }, [load]);

  const toggle = (id: string) => setOpen(flip(id));

  return (
    <>
      <div className="filters">
        <TimeRange {...time.props} />
        <Select caption="Type" value={type} onChange={setType} options={[{ value: "", label: "All" }, ...Object.entries(TARGETS).map(([value, label]) => ({ value, label }))]} />
        <TextInput caption="Name" type="search" placeholder="target or who" value={search} onChange={(e) => setSearch(e.target.value)} />
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>When</th>
              <th>Who</th>
              <th>Action</th>
              <th>Target</th>
              <th>Changes</th>
              <th aria-label="Details" />
            </tr>
          </thead>
          <tbody>
            {rows.map((e) => {
              const hasDetails = Object.keys(e.details).length > 0;
              const expanded = open.has(e.id);
              const changes = fieldChanges(e);
              return (
                <Fragment key={e.id}>
                  <tr>
                    <td className="when" title={dateTime(e.ts)}>{relative(e.ts)}</td>
                    <td>
                      {e.actor_name ?? "–"} <span className="muted small">{ACTORS[e.actor_kind] ?? e.actor_kind}</span>
                    </td>
                    <td>
                      <Badge tone={actionTone(e.action)}>{ACTIONS[e.action] ?? e.action}</Badge>
                    </td>
                    <td>
                      {TARGETS[e.target_type] ?? e.target_type}
                      {e.target_name && <span className="mono"> {e.target_name}</span>}
                    </td>
                    <td className="error-cell" title={summary(e)}>
                      {summary(e)}
                    </td>
                    <td className="num">
                      {hasDetails && (
                        <Button variant="ghost" className="details-btn" onClick={() => toggle(e.id)} aria-expanded={expanded}>
                          {expanded ? "Hide details" : "Details"}
                        </Button>
                      )}
                    </td>
                  </tr>
                  {expanded && (
                    <tr className="child-row">
                      <td colSpan={6}>
                        {changes ? (
                          <table className="detail-table">
                            <thead>
                              <tr>
                                <th>Field</th>
                                <th>Before</th>
                                <th>After</th>
                              </tr>
                            </thead>
                            <tbody>
                              {changes.map(([field, c]) => (
                                <tr key={field}>
                                  <td className="mono">{field}</td>
                                  <td>{value(c.from)}</td>
                                  <td>{value(c.to)}</td>
                                </tr>
                              ))}
                            </tbody>
                          </table>
                        ) : (
                          <pre className="raw">{JSON.stringify(e.details, null, 2)}</pre>
                        )}
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
            {rows.length === 0 && (
              <tr>
                <td colSpan={6} className="empty">
                  No changes match these filters.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </>
  );
}
