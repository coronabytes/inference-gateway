import { useCallback, useEffect, useState } from "react";
import { useIsUser } from "../access";
import {
  api,
  callsModels,
  loadPages,
  setListParam,
  type BudgetPeriod,
  type GatewaySettingsView,
  type Guardrail,
  type KeyInput,
  type KeyScope,
  type NamedItem,
  type Project,
  type PromptLogging,
  type TagInfo,
  type VirtualKey,
} from "../api";
import { Banner, Button, MenuButton, MultiSelect, PageActions, Select, Tabs, TextInput, Toggle, useDebounced, useHashTab } from "../components/controls";
import { GuardrailList, TagList, TagPicker } from "../components/Tags";
import { KeySignIns, takeSignInResult } from "./KeySignIns";
import {
  Badge,
  BudgetMeter,
  ConfirmDialog,
  EditIcon,
  ErrorBanner,
  IconButton,
  MenuIcon,
  Modal,
  PauseIcon,
  PlayIcon,
  PlugIcon,
  ResetIcon,
  optInt,
  optNum,
  type ConfirmRequest,
} from "../components/ui";
import { offerDeleteStoredPrompts } from "../components/StoredPrompts";
import { dateTime } from "../format";
import { ProjectForm, confirmDelete, confirmResetSpend, setProjectEnabled } from "./Projects";
import { Admins } from "./Settings";

/** A key being created, in `project` when started from a project's card, or one being edited. */
type Editing = { mode: "create"; project?: string } | { mode: "edit"; key: VirtualKey };


const SCOPE_LABEL: Record<KeyScope, string> = { inference: "Inference", admin: "Admin", all: "Inference + admin", project: "Project admin" };

/** The scopes a key may have: in a project, none with every right an admin has. */
const scopesFor = (inProject: boolean): KeyScope[] => (inProject ? ["inference", "project"] : ["inference", "admin", "all"]);

const SCOPE_HINT: Record<KeyScope, string> = {
  inference: "Calls models through the inference API (/v1/…).",
  admin: "Calls the admin API (/admin/api/…) with the same rights as the master key, including managing keys and credentials.",
  all: "Calls the admin API (/admin/api/…) with the same rights as the master key, including managing keys and credentials. Also calls models through the inference API.",
  project:
    "Calls the admin API (/admin/api/…) for this project only, with the rights of a user who works in it: its keys, playground, request log and metrics. It does not call models.",
};

/** The tag filter's value for keys without tags; tags never contain spaces. */
const NO_TAGS = " none";

/** The project filter's value for keys without a project. */
const NO_PROJECT = " none";

const TABS = ["keys", "users"] as const;

/** Keys, and project cards, shown at first and added by each "Show more". */
const PAGE = 100;

/** Projects by name, each once. */
function byName(lists: Project[][]): Project[] {
  const all = new Map(lists.flat().map((p) => [p.id, p]));
  return [...all.values()].sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()) || a.id.localeCompare(b.id));
}

export function Keys() {
  const [tab, setTab] = useHashTab("access", TABS);
  const [projectFilter, setProjectFilter] = useState<string[]>([]);
  // Users see their projects' keys only; they do not change the projects themselves.
  if (useIsUser()) {
    return (
      <div className="page">
        <KeyList projectFilter={projectFilter} onProjectFilter={setProjectFilter} />
      </div>
    );
  }
  return (
    <div className="page">
      <Tabs
        label="Access"
        options={[
          { value: "keys", label: "Keys & projects" },
          { value: "users", label: "Users" },
        ]}
        value={tab}
        onChange={setTab}
      />
      {tab === "users" ? <Admins /> : <KeyList projectFilter={projectFilter} onProjectFilter={setProjectFilter} />}
    </div>
  );
}

function KeyList({ projectFilter, onProjectFilter }: { projectFilter: string[]; onProjectFilter: (ids: string[]) => void }) {
  const user = useIsUser();
  const [keys, setKeys] = useState<VirtualKey[]>([]);
  const [moreKeys, setMoreKeys] = useState(false);
  // Project cards: a page of projects by name, and the projects of shown keys beyond it.
  const [projectList, setProjects] = useState<Project[]>([]);
  const [moreProjects, setMoreProjects] = useState(false);
  const [projectLimit, setProjectLimit] = useState(PAGE);
  const [keyProjects, setKeyProjects] = useState<Project[]>([]);
  const projects = byName([projectList, keyProjects]);
  // Every project's name, for the filter. Null until loaded, so that a project filter is kept
  // until it can be checked.
  const [projectNames, setProjectNames] = useState<NamedItem[] | null>(null);
  const [tags, setTags] = useState<TagInfo[]>([]);
  // Only admins choose guardrails, and only they can list them.
  const [guardrails, setGuardrails] = useState<Guardrail[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [issued, setIssued] = useState<{ title: string; key: string } | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);
  const [signIns, setSignIns] = useState<VirtualKey | null>(null);
  // A key's sign-in to an MCP server or A2A agent comes back here.
  const [signedIn, setSignedIn] = useState(takeSignInResult);
  const [editingProject, setEditingProject] = useState<Project | "new" | null>(null);
  const [query, setQuery] = useState("");
  const [tagFilter, setTagFilter] = useState<string[]>([]);
  const [loggingOnly, setLoggingOnly] = useState(false);
  // The gateway's prompt logging policy for keys outside projects; users' keys are all in projects.
  const [gatewayLogging, setGatewayLogging] = useState<PromptLogging>("deny");
  const search = useDebounced(query.trim());

  // A filter for a project deleted meanwhile matches nothing; it is dropped instead.
  const projectIds = new Set((projectNames ?? []).map((p) => p.id));
  const projectsPicked = projectNames ? projectFilter.filter((id) => id === NO_PROJECT || projectIds.has(id)) : projectFilter;
  // The server filters the keys.
  const keyQuery = new URLSearchParams();
  if (search) keyQuery.set("q", search);
  setListParam(keyQuery, "tags", tagFilter, NO_TAGS);
  setListParam(keyQuery, "projects", projectsPicked, NO_PROJECT);
  if (loggingOnly) keyQuery.set("logging", "true");
  const filters = keyQuery.toString().replace(/%2C/g, ",");

  // How many keys to show: a page more with each "Show more", back to one when the filters change.
  const [keyLimit, setKeyLimit] = useState({ filters, n: PAGE });
  const keyCount = keyLimit.filters === filters ? keyLimit.n : PAGE;

  const loadKeys = useCallback(async () => {
    try {
      const { items, more } = await loadPages<VirtualKey>(`/keys${filters ? `?${filters}` : ""}`, keyCount);
      setKeys(items);
      setMoreKeys(more);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [filters, keyCount]);

  const loadProjects = useCallback(async () => {
    try {
      const [{ items, more }, names] = await Promise.all([
        loadPages<Project>("/projects", projectLimit),
        api<NamedItem[]>("/projects/options"),
      ]);
      setProjects(items);
      setMoreProjects(more);
      setProjectNames(names);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [projectLimit]);

  // Cards for shown keys whose project is not on the loaded pages.
  useEffect(() => {
    const loaded = new Set(projectList.map((p) => p.id));
    const missing = [...new Set(keys.map((k) => k.project_id).filter((id): id is string => !!id && !loaded.has(id)))];
    if (missing.length === 0) {
      setKeyProjects([]);
      return;
    }
    api<Project[]>(`/projects?ids=${missing.join(",")}`)
      .then(setKeyProjects)
      .catch((e) => setError((e as Error).message));
  }, [keys, projectList]);

  const showMore = () => {
    if (moreKeys) setKeyLimit({ filters, n: keyCount + PAGE });
    if (moreProjects) setProjectLimit((n) => n + PAGE);
  };

  // What the cards and the editors need besides the keys: not reloaded as the filters change.
  const loadContext = useCallback(async () => {
    try {
      const [t, g, s] = await Promise.all([
        api<TagInfo[]>("/tags"),
        user ? Promise.resolve([]) : api<Guardrail[]>("/guardrails"),
        user ? Promise.resolve(null) : api<GatewaySettingsView>("/settings"),
      ]);
      setGuardrails(g);
      if (s) setGatewayLogging(s.settings.prompt_logging);
      setTags(t);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [user]);

  const load = useCallback(async () => {
    await Promise.all([loadKeys(), loadProjects(), loadContext()]);
  }, [loadKeys, loadProjects, loadContext]);

  useEffect(() => {
    loadKeys();
  }, [loadKeys]);

  useEffect(() => {
    loadProjects();
  }, [loadProjects]);

  useEffect(() => {
    loadContext();
  }, [loadContext]);

  const act = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      await load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  // The UI restricts keys by tags only; saving from it clears any allowed_models list.
  const toInput = (k: VirtualKey): KeyInput => ({
    name: k.name,
    scope: k.scope,
    project_id: k.project_id,
    allowed_models: [],
    allowed_tags: k.allowed_tags,
    rpm_limit: k.rpm_limit,
    tpm_limit: k.tpm_limit,
    max_budget_usd: k.max_budget_usd,
    budget_period: k.budget_period,
    expires_at: k.expires_at,
    enabled: k.enabled,
    guardrail_ids: k.guardrail_ids,
    web_search: k.web_search,
    prompt_logging: k.prompt_logging,
  });

  // Tags the filter offers: those deployments, routers and MCP servers carry, and those keys still hold.
  const tagOptions = tags.map((t) => t.tag).sort();
  // Keys pick only tags that deployments, routers or MCP servers carry.
  const pickableTags = tags.filter((t) => t.deployments + t.routers + t.mcp_servers > 0).map((t) => t.tag);
  // Guardrail names by id, for the grid; empty for users, who cannot list them.
  const guardrailNames = new Map(guardrails.map((g) => [g.id, g.name]));

  const shown = keys;
  // Filters on the keys themselves; the project filter picks cards.
  const keyFiltering = !!search || tagFilter.length > 0 || loggingOnly;
  const filtering = keyFiltering || projectsPicked.length > 0;

  // With projects, each one gets a card with its keys, and a last card holds the keys without a
  // project. A project without keys still shows, unless the keys are filtered.
  const groups = [
    ...projects.map((p) => ({ id: p.id, name: p.name, description: p.description, project: p as Project | null })),
    { id: NO_PROJECT, name: "No project", description: null, project: null },
  ]
    .map((g) => ({ ...g, keys: shown.filter((k) => (k.project_id ?? NO_PROJECT) === g.id) }))
    .filter((g) => g.keys.length > 0 || (g.project && !keyFiltering && (projectsPicked.length === 0 || projectsPicked.includes(g.id))));
  // A new key starts in the project the list is filtered to, if just one.
  const filteredProject = projectsPicked.length === 1 && projectsPicked[0] !== NO_PROJECT ? projectsPicked[0] : null;

  // A project card's header, on the keys' grid: the project's name and description over the
  // keys' name and access, then its budget, state and commands over theirs.
  const cardHead = (g: (typeof groups)[number]) => {
    const p = g.project;
    return (
      <header className="list-head on-grid">
        <div className="list-title">
          <h3>{g.name}</h3>
          {g.description && <p className="muted small">{g.description}</p>}
          {p && <GuardrailList ids={p.guardrail_ids} names={guardrailNames} />}
        </div>
        {p && (
          <>
            <div className="key-budget">{p.budget ? <BudgetMeter budget={p.budget} /> : <span className="muted small">No budget</span>}</div>
            <div className="stat">
              {p.enabled ? <Badge tone="good">Active</Badge> : <Badge tone="warning">Inactive</Badge>}
              {p.prompt_logging === "force" && (
                <span title="Every key's prompts and responses are logged">
                  <Badge tone="warning">Logging</Badge>
                </span>
              )}
            </div>
            {/* Only admins change projects. */}
            <div className="actions">
              {!user && (
                <>
                  <IconButton label={`Edit project ${p.name}`} title="Edit project" onClick={() => setEditingProject(p)}>
                    <EditIcon />
                  </IconButton>
                  <MenuButton
                    className="icon-btn"
                    label={`More actions for project ${p.name}`}
                    items={[
                      { label: "New key in this project", onClick: () => setEditing({ mode: "create", project: p.id }) },
                      // While inactive, every key in the project is refused, on the inference API and the admin API.
                      { label: p.enabled ? "Deactivate" : "Activate", onClick: () => act(() => setProjectEnabled(p, !p.enabled, setConfirming, load)) },
                      { label: "Reset spend", onClick: () => confirmResetSpend(p, setConfirming, load), disabled: !p.budget },
                      {
                        label: "Delete stored prompts",
                        onClick: () => offerDeleteStoredPrompts(`/projects/${p.id}`, p.name, setConfirming, { always: true, done: load }).catch((e) => setError((e as Error).message)),
                      },
                      { label: "Delete", onClick: () => confirmDelete(p, setConfirming, load), danger: true },
                    ]}
                  >
                    <MenuIcon />
                  </MenuButton>
                </>
              )}
            </div>
          </>
        )}
      </header>
    );
  };

  // A key: who it is, what it may call, its budget and spend, its state, and its commands. The
  // parts line up from key to key through a shared grid, without the look of a table.
  const keyItem = (k: VirtualKey) => {
    const expired = k.expires_at != null && new Date(k.expires_at) < new Date();
    const b = k.budget;
    const frac = b ? Math.min(1, b.limit_usd > 0 ? b.spent_usd / b.limit_usd : 1) : null;
    // Tags, limits and budgets only apply to inference calls.
    const adminOnly = !callsModels(k.scope);
    const pb = projects.find((p) => p.id === k.project_id)?.budget;
    const projectSpent = !adminOnly && !!pb && pb.spent_usd >= pb.limit_usd;
    return (
      <li key={k.id} className="list-item">
        <div className="key-ident">
          <div className="mono key-name-text">{k.name}</div>
          <div className="mono muted small">{k.key_prefix}</div>
        </div>
        {/* What the key may call: its scope, and for inference the tags it holds. */}
        <div className="key-access">
          <div className="key-scope">{SCOPE_LABEL[k.scope]}</div>
          {!adminOnly && <TagList tags={k.allowed_tags} empty="all models" />}
          {!adminOnly && (
            <GuardrailList ids={k.guardrail_ids} inherited={projects.find((p) => p.id === k.project_id)?.guardrail_ids} names={guardrailNames} />
          )}
        </div>
        <div className="key-budget">
          {adminOnly ? <span className="muted small">–</span> : b ? <BudgetMeter budget={b} /> : <span className="muted small">No budget</span>}
        </div>
        <div className="stat">
          {!k.project_active ? (
            <Badge tone="warning">Project inactive</Badge>
          ) : !k.enabled ? (
            <Badge tone="warning">Inactive</Badge>
          ) : expired ? (
            <Badge tone="warning">Expired</Badge>
          ) : frac != null && frac >= 1 ? (
            <Badge tone="critical">Budget hit</Badge>
          ) : projectSpent ? (
            <Badge tone="critical">Project budget hit</Badge>
          ) : (
            <Badge tone="good">Active</Badge>
          )}
          {k.expires_at && !expired && <span className="muted small">expires {dateTime(k.expires_at)}</span>}
          {k.prompt_logged && (
            <span title="Its prompts and responses are logged">
              <Badge tone="warning">Logging</Badge>
            </span>
          )}
        </div>
        <div className="actions">
          <IconButton label={`Edit ${k.name}`} title="Edit" onClick={() => setEditing({ mode: "edit", key: k })}>
            <EditIcon />
          </IconButton>
          <IconButton
            label={`${k.enabled ? "Disable" : "Enable"} ${k.name}`}
            title={k.enabled ? "Disable" : "Enable"}
            onClick={() => {
              const save = () => act(() => api(`/keys/${k.id}`, { method: "PUT", json: { ...toInput(k), enabled: !k.enabled } }));
              // Disabling a key in use cuts its clients off, so it asks first; enabling, or disabling
              // a key its inactive project already blocks, does not.
              if (!k.enabled || !k.project_active) return save();
              setConfirming({
                title: "Disable key",
                message: <>Disable <strong>{k.name}</strong>? Clients using it are refused immediately, on every replica, until it is enabled again.</>,
                confirmLabel: "Disable",
                action: save,
              });
            }}
          >
            {k.enabled ? <PauseIcon /> : <PlayIcon />}
          </IconButton>
          <IconButton label={`Sign-ins of ${k.name}`} title="Sign-ins: to MCP servers and A2A agents, and of apps as this key" onClick={() => setSignIns(k)}>
            <PlugIcon />
          </IconButton>
          <IconButton
            label={`Regenerate ${k.name}`}
            title="Regenerate key"
            onClick={() =>
              setConfirming({
                title: "Regenerate key",
                message: (
                  <>
                    Issue a new secret for <strong>{k.name}</strong>? The current secret stops working immediately, on every replica.
                    Limits, budget and spend stay with the key.
                  </>
                ),
                confirmLabel: "Regenerate",
                action: () =>
                  act(async () => {
                    const r = await api<{ key: string }>(`/keys/${k.id}/regenerate`, { method: "POST" });
                    setIssued({ title: "Key regenerated", key: r.key });
                  }),
              })
            }
          >
            <ResetIcon />
          </IconButton>
        </div>
      </li>
    );
  };

  return (
    <>
      <div className="page-head">
        {(keys.length > 0 || filtering) && (
          <div className="filters">
            <TextInput caption="Search" type="search" value={query} onChange={(e) => setQuery(e.target.value)} placeholder="name or key" />
            {(projectNames ?? []).length > 0 && (
              <MultiSelect
                caption="Project"
                // A user's keys are always in one of their projects.
                options={[...(projectNames ?? []).map((p) => ({ value: p.id, label: p.name })), ...(user ? [] : [{ value: NO_PROJECT, label: "No project" }])]}
                values={projectsPicked}
                onChange={onProjectFilter}
              />
            )}
            <MultiSelect
              caption="Tags"
              options={[...tagOptions.map((t) => ({ value: t, label: t })), { value: NO_TAGS, label: "No tags" }]}
              values={tagFilter}
              onChange={setTagFilter}
            />
            <Toggle caption="Logging" hint="Only the keys whose prompts and responses are logged now." checked={loggingOnly} onChange={setLoggingOnly} />
            {filtering && (
              <span className="muted small">
                {shown.length}
                {moreKeys ? "+ keys match" : shown.length === 1 ? " key matches" : " keys match"}
              </span>
            )}
          </div>
        )}
        <PageActions
          actions={[
            // Only admins create projects.
            ...(user ? [] : [{ label: "New project", onClick: () => setEditingProject("new" as const) }]),
            { label: "New key", onClick: () => setEditing({ mode: "create" }), primary: true },
          ]}
        />
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {signedIn && (
        <Banner tone={signedIn.tone} onDismiss={() => setSignedIn(null)}>
          {signedIn.message}
        </Banner>
      )}
      {signIns && <KeySignIns keyItem={signIns} onClose={() => setSignIns(null)} />}
      {projects.length > 0 && groups.length > 0 ? (
        groups.map((g) => (
          // An inactive project's keys are refused; its card shows them greyed out.
          <section key={g.id} className={g.project && !g.project.enabled ? "card list-card key-group inactive" : "card list-card key-group"}>
            {cardHead(g)}
            {g.keys.length > 0 ? (
              <ul className="list">{g.keys.map(keyItem)}</ul>
            ) : (
              <p className="empty">{moreKeys ? "Its keys come with Show more." : "No keys in this project yet."}</p>
            )}
          </section>
        ))
      ) : shown.length === 0 ? (
        <div className="card list-card">
          <p className="empty">{filtering ? "No keys match the filters." : "No keys or projects yet. Create a project to group keys by team, app or customer."}</p>
        </div>
      ) : (
        <section className="card list-card key-group">
          <ul className="list">{shown.map(keyItem)}</ul>
        </section>
      )}
      {(moreKeys || moreProjects) && (
        <div className="panel-actions">
          <Button onClick={showMore}>Show more</Button>
        </div>
      )}

      {editing && (
        <KeyForm
          tags={pickableTags}
          projects={projects}
          guardrails={guardrails}
          gatewayLogging={gatewayLogging}
          user={user}
          defaultProject={(editing.mode === "create" && editing.project) || filteredProject}
          initial={editing.mode === "edit" ? toInput(editing.key) : undefined}
          onClose={() => setEditing(null)}
          onDelete={
            editing.mode === "edit"
              ? () => {
                  const k = editing.key;
                  setEditing(null);
                  setConfirming({
                    title: "Delete virtual key",
                    message: (
                      <>
                        Delete <strong>{k.name}</strong>? Clients using this key stop working immediately, on every replica. Prompts and responses it
                        logged are deleted too.
                      </>
                    ),
                    action: () =>
                      act(async () => {
                        await api(`/keys/${k.id}/bodies`, { method: "DELETE" });
                        await api(`/keys/${k.id}`, { method: "DELETE" });
                      }),
                  });
                }
              : undefined
          }
          onDeleteStoredPrompts={
            editing.mode === "edit"
              ? () => {
                  const k = editing.key;
                  setEditing(null);
                  offerDeleteStoredPrompts(`/keys/${k.id}`, k.name, setConfirming, { always: true, done: load }).catch((e) => setError((e as Error).message));
                }
              : undefined
          }
          onSave={async (input) => {
            if (editing.mode === "create") {
              const r = await api<{ key: string }>("/keys", { method: "POST", json: input });
              setIssued({ title: "Key created", key: r.key });
            } else {
              await api(`/keys/${editing.key.id}`, { method: "PUT", json: input });
            }
            setEditing(null);
            await load();
            // Logging just turned off: offer to delete what it stored.
            if (editing.mode === "edit" && editing.key.prompt_logging && !input.prompt_logging) {
              await offerDeleteStoredPrompts(`/keys/${editing.key.id}`, input.name, setConfirming, { done: load });
            }
          }}
        />
      )}

      {editingProject && (
        <ProjectForm
          initial={editingProject === "new" ? undefined : editingProject}
          confirm={setConfirming}
          onClose={() => setEditingProject(null)}
          onSaved={async () => {
            setEditingProject(null);
            await load();
          }}
        />
      )}
      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
      {issued && (
        <Modal title={issued.title} onClose={() => setIssued(null)} footer={<button type="button" onClick={() => setIssued(null)}>Done</button>}>
          <p>Copy this key now. It is stored only as a hash and cannot be shown again.</p>
          <div className="secret">
            <code>{issued.key}</code>
            <button type="button" className="primary" onClick={() => navigator.clipboard?.writeText(issued.key)}>Copy</button>
          </div>
        </Modal>
      )}
    </>
  );
}

function KeyForm({
  tags,
  projects,
  guardrails,
  gatewayLogging,
  user,
  defaultProject,
  initial,
  onSave,
  onDelete,
  onDeleteStoredPrompts,
  onClose,
}: {
  /** Tags deployments carry; keys can only pick from these. */
  tags: string[];
  projects: Project[];
  /** Empty for users, who do not choose guardrails. */
  guardrails: Guardrail[];
  /** Prompt logging for keys outside projects. */
  gatewayLogging: PromptLogging;
  /** A user's keys are in one of their projects. */
  user: boolean;
  /** A new key's project. */
  defaultProject: string | null;
  initial?: KeyInput;
  onSave: (k: KeyInput) => Promise<void>;
  /** Offered when editing: closes the dialog and asks for confirmation. */
  onDelete?: () => void;
  /** Offered when editing: closes the dialog and offers to delete the key's stored prompts. */
  onDeleteStoredPrompts?: () => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [scope, setScope] = useState<KeyScope>(initial?.scope ?? "inference");
  const [project, setProject] = useState(initial ? (initial.project_id ?? "") : (defaultProject ?? (user ? (projects[0]?.id ?? "") : "")));
  const [allowedTags, setAllowedTags] = useState<string[]>(initial?.allowed_tags ?? []);
  const [guardrailIds, setGuardrailIds] = useState<string[]>(initial?.guardrail_ids ?? []);
  const [webSearch, setWebSearch] = useState(initial?.web_search ?? true);
  const [promptLogging, setPromptLogging] = useState(initial?.prompt_logging ?? false);
  const [rpm, setRpm] = useState(initial?.rpm_limit?.toString() ?? "");
  const [tpm, setTpm] = useState(initial?.tpm_limit?.toString() ?? "");
  const [period, setPeriod] = useState<BudgetPeriod | "">(initial?.budget_period ?? "");
  const [budget, setBudget] = useState(initial?.max_budget_usd?.toString() ?? "");
  const [expires, setExpires] = useState(initial?.expires_at ? initial.expires_at.slice(0, 10) : "");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Moving a key into a project narrows an admin key to the project; out of one, a project admin
  // key falls back to inference rather than gaining every right an admin has.
  const changeProject = (p: string) => {
    setProject(p);
    if (p && !scopesFor(true).includes(scope)) setScope("project");
    if (!p && !scopesFor(false).includes(scope)) setScope("inference");
  };

  // Tags, limits and budget only apply to inference calls; an admin-only key saves without them.
  const inference = callsModels(scope);
  const projectGuardrails = guardrails.filter((g) => projects.find((p) => p.id === project)?.guardrail_ids.includes(g.id)).map((g) => g.name);
  // A project that forbids web search forbids it for its keys, whatever theirs says.
  const projectForbidsSearch = projects.find((p) => p.id === project)?.web_search === false;
  // The project's prompt logging policy decides, the gateway's for keys outside projects; a key
  // only chooses where it is allowed.
  const loggingPolicy = project ? (projects.find((p) => p.id === project)?.prompt_logging ?? "deny") : gatewayLogging;
  const logged = loggingPolicy === "force" || (loggingPolicy === "allow" && promptLogging);
  const loggingHint = {
    deny: project
      ? "The key's project denies prompt logging, so none of its keys can turn it on."
      : "Prompt logging is denied for keys outside projects (Cluster → Settings).",
    allow:
      "For debugging, not for production keys: stores this key's full prompts, guardrail results and responses, unencrypted. It stays on until it is turned off.",
    force: "The key's project logs the prompts and responses of all its keys, whatever the key says.",
  }[loggingPolicy];
  const amount = optNum(budget);
  const budgetMissing = inference && period !== "" && amount == null;

  const submit = async () => {
    setBusy(true);
    try {
      await onSave({
        name,
        scope,
        project_id: project || null,
        allowed_models: [],
        allowed_tags: inference ? allowedTags : [],
        rpm_limit: inference ? optInt(rpm) : null,
        tpm_limit: inference ? optInt(tpm) : null,
        max_budget_usd: inference && period ? amount : null,
        budget_period: (inference && period) || null,
        expires_at: expires ? new Date(`${expires}T23:59:59Z`).toISOString() : null,
        enabled: initial?.enabled ?? true,
        // Users keep the key's guardrails as they are.
        ...(guardrails.length > 0 && { guardrail_ids: inference ? guardrailIds : [] }),
        // Kept while the project forbids it, so it applies again once the project allows it.
        web_search: webSearch,
        // Likewise kept while the project denies or forces logging.
        prompt_logging: promptLogging,
      });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={initial ? "Edit key" : "New key"}
      onClose={onClose}
      footer={
        <>
          {onDelete && (
            <Button variant="danger" onClick={onDelete}>
              Delete
            </Button>
          )}
          {onDeleteStoredPrompts && (
            <Button variant="ghost" onClick={onDeleteStoredPrompts}>
              Delete stored prompts
            </Button>
          )}
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy || !name.trim() || budgetMissing} onClick={submit}>
            {initial ? "Save" : "Create key"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="form">
        <TextInput caption="Name" value={name} onChange={(e) => setName(e.target.value)} autoFocus placeholder="e.g. team-search-prod" required />
        {projects.length > 0 && (
          <Select
            caption="Project"
            hint="Groups the key on the Access page. While the project is inactive, the key is refused."
            value={project}
            onChange={changeProject}
            options={[
              ...(user ? [] : [{ value: "", label: "None" }]),
              ...projects.map((p) => ({ value: p.id, label: p.name, note: p.enabled ? undefined : "inactive" })),
            ]}
          />
        )}
        <Select
          caption="Scope"
          hint={SCOPE_HINT[scope]}
          value={scope}
          onChange={(s) => setScope(s as KeyScope)}
          options={scopesFor(!!project).map((s) => ({ value: s, label: SCOPE_LABEL[s] }))}
        />
        {inference && (
          <>
            <TagPicker
              caption="Tags"
              hint={
                allowedTags.length
                  ? "This key can use deployments, routers, MCP servers and A2A agents without tags, and those whose tags are all selected here."
                  : "No tags selected: this key can use every model, MCP server and A2A agent, except those limited to other projects."
              }
              options={tags}
              value={allowedTags}
              onChange={setAllowedTags}
            />
            {guardrails.length > 0 && (
              <MultiSelect
                caption="Guardrails"
                hint={
                  projectGuardrails.length
                    ? `Every request of this key goes through these, and through its project's: ${projectGuardrails.join(", ")}.`
                    : "Every request of this key goes through these, whatever it asks for. Requests can ask for more by name."
                }
                values={guardrailIds}
                onChange={setGuardrailIds}
                none="None"
                options={guardrails.map((g) => ({ value: g.id, label: g.name, note: g.description }))}
              />
            )}
            <Toggle
              caption="Web search"
              hint={
                projectForbidsSearch
                  ? "The key's project forbids web search, so none of its keys can turn it on."
                  : "Whether requests may have the model search the web, billed per search at the deployment's price. Off, they are refused."
              }
              checked={webSearch && !projectForbidsSearch}
              onChange={setWebSearch}
              disabled={projectForbidsSearch}
            />
            <Toggle
              caption="Prompt logging"
              hint={loggingHint}
              checked={logged}
              onChange={setPromptLogging}
              disabled={loggingPolicy !== "allow"}
            />
            {loggingPolicy === "allow" && promptLogging && !initial?.prompt_logging && (
              <Banner tone="warning">
                This key's prompts and responses will be stored, unencrypted, until logging is turned off. Use it for debugging, not for production traffic.
              </Banner>
            )}
            <div className="row">
              <TextInput caption="Requests / minute" inputMode="numeric" value={rpm} onChange={(e) => setRpm(e.target.value)} placeholder="unlimited" />
              <TextInput caption="Tokens / minute" inputMode="numeric" value={tpm} onChange={(e) => setTpm(e.target.value)} placeholder="unlimited" />
            </div>
            <div className="row">
              <Select
                caption="Budget"
                hint={period ? "Resets at the start of each UTC day, week (Monday) or month." : undefined}
                value={period}
                onChange={(p) => setPeriod(p as BudgetPeriod | "")}
                options={[
                  { value: "", label: "Unlimited" },
                  { value: "day", label: "Per day" },
                  { value: "week", label: "Per week" },
                  { value: "month", label: "Per month" },
                ]}
              />
              {period ? (
                <TextInput caption="Amount (USD)" inputMode="decimal" value={budget} onChange={(e) => setBudget(e.target.value)} placeholder="required" required autoFocus={!initial} />
              ) : (
                <div />
              )}
            </div>
          </>
        )}
        <TextInput caption="Expires" type="date" value={expires} onChange={(e) => setExpires(e.target.value)} />
      </div>
    </Modal>
  );
}
