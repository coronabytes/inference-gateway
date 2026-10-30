import { useCallback, useEffect, useState } from "react";
import { api, type BudgetPeriod, type Guardrail, type Project, type ProjectInput, type PromptLogging } from "../api";
import { Banner, Button, MenuButton, MultiSelect, PageActions, Segmented, Select, TextArea, TextInput, Toggle, useDebounced } from "../components/controls";
import { Badge, BudgetMeter, ConfirmDialog, EditIcon, ErrorBanner, IconButton, MenuIcon, Modal, optNum, type ConfirmRequest } from "../components/ui";
import { offerDeleteStoredPrompts } from "../components/StoredPrompts";
import { dateTime } from "../format";

/** The project as it is now, as input for saving it with `changes`. */
function input(p: Project, changes: Partial<ProjectInput>): ProjectInput {
  return {
    name: p.name,
    description: p.description,
    enabled: p.enabled,
    max_budget_usd: p.max_budget_usd,
    budget_period: p.budget_period,
    web_search: p.web_search,
    prompt_logging: p.prompt_logging,
    ...changes,
  };
}

/**
 * Activates a project at once; deactivating asks first (through `confirm`), since every key in it
 * stops working. `done` runs after the change is saved.
 */
export async function setProjectEnabled(p: Project, enabled: boolean, confirm: (r: ConfirmRequest) => void, done: () => Promise<void>) {
  const save = async () => {
    await api(`/projects/${p.id}`, { method: "PUT", json: input(p, { enabled }) });
    await done();
  };
  if (enabled) return save();
  confirm({
    title: "Deactivate project",
    message: (
      <>
        Deactivate <strong>{p.name}</strong>?{" "}
        {p.keys
          ? `${p.keys === 1 ? "Its key stops" : `All ${p.keys} of its keys stop`} working immediately, on every replica, until the project is activated again.`
          : "Keys added to it will not work until it is activated again."}
      </>
    ),
    confirmLabel: "Deactivate",
    action: save,
  });
}

/** Asks to start the project's current budget period over; `done` runs after the reset. */
export function confirmResetSpend(p: Project, confirm: (r: ConfirmRequest) => void, done: () => Promise<void>) {
  confirm({
    title: "Reset project spend",
    message: (
      <>
        Start the current budget period of <strong>{p.name}</strong> over? Spend so far no longer counts against the project's budget. Its keys' own budgets
        are not affected.
      </>
    ),
    confirmLabel: "Reset",
    action: async () => {
      await api(`/projects/${p.id}/reset-spend`, { method: "POST" });
      await done();
    },
  });
}

/** Asks to delete the project and its keys. `done` runs after the delete. */
export function confirmDelete(p: Project, confirm: (r: ConfirmRequest) => void, done: () => Promise<void>) {
  confirm({
    title: "Delete project",
    message: (
      <>
        Delete the project <strong>{p.name}</strong>?{" "}
        {p.keys
          ? `Its ${p.keys === 1 ? "key is" : `${p.keys} keys are`} deleted with it and stop${p.keys === 1 ? "s" : ""} working.`
          : "It groups no keys."}{" "}
        Prompts and responses its requests logged are deleted too.
      </>
    ),
    action: async () => {
      await api(`/projects/${p.id}/bodies`, { method: "DELETE" });
      await api(`/projects/${p.id}`, { method: "DELETE" });
      await done();
    },
  });
}

/** Projects group keys on the Keys tab. An inactive one blocks its keys, and a budget caps their spend on top of their own. */
export function Projects({ onShowKeys }: { onShowKeys: (project: Project) => void }) {
  const [projects, setProjects] = useState<Project[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Project | "new" | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);
  const [query, setQuery] = useState("");
  const search = useDebounced(query.trim());

  // The server filters the projects.
  const load = useCallback(async () => {
    try {
      setProjects(await api<Project[]>(`/projects${search ? `?q=${encodeURIComponent(search)}` : ""}`));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [search]);

  useEffect(() => {
    load();
  }, [load]);

  const askDelete = (p: Project) => confirmDelete(p, setConfirming, load);
  const askReset = (p: Project) => confirmResetSpend(p, setConfirming, load);

  const toggle = async (p: Project) => {
    try {
      await setProjectEnabled(p, !p.enabled, setConfirming, load);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const shown = projects;

  return (
    <>
      <div className="page-head">
        {(projects.length > 0 || !!search) && (
          <div className="filters">
            <TextInput caption="Search" type="search" value={query} onChange={(e) => setQuery(e.target.value)} placeholder="name or description" />
          </div>
        )}
        <PageActions actions={[{ label: "New project", onClick: () => setEditing("new"), primary: true }]} />
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Description</th>
              <th className="num">Keys</th>
              <th>Budget</th>
              <th>Status</th>
              <th>Created</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {shown.map((p) => (
              <tr key={p.id}>
                <td className="mono">{p.name}</td>
                <td className="muted">{p.description ?? "–"}</td>
                <td className="num">
                  {p.keys ? (
                    <button className="link" onClick={() => onShowKeys(p)}>
                      {p.keys}
                    </button>
                  ) : (
                    0
                  )}
                </td>
                <td>{p.budget ? <BudgetMeter budget={p.budget} /> : <span className="muted">unlimited</span>}</td>
                <td>
                  {p.enabled ? <Badge tone="good">Active</Badge> : <Badge tone="warning">Inactive</Badge>}
                  {p.prompt_logging === "force" && (
                    <span title="Every key's prompts and responses are logged">
                      {" "}
                      <Badge tone="warning">Logging</Badge>
                    </span>
                  )}
                </td>
                <td className="when">{dateTime(p.created_at)}</td>
                <td className="actions">
                  <IconButton label={`Edit ${p.name}`} title="Edit" onClick={() => setEditing(p)}>
                    <EditIcon />
                  </IconButton>
                  <MenuButton
                    className="icon-btn"
                    label={`More actions for ${p.name}`}
                    items={[
                      { label: "Show keys", onClick: () => onShowKeys(p), disabled: !p.keys },
                      { label: p.enabled ? "Deactivate" : "Activate", onClick: () => toggle(p) },
                      { label: "Reset spend", onClick: () => askReset(p), disabled: !p.budget },
                      {
                        label: "Delete stored prompts",
                        onClick: () => offerDeleteStoredPrompts(`/projects/${p.id}`, p.name, setConfirming, { always: true, done: load }).catch((e) => setError((e as Error).message)),
                      },
                      { label: "Delete", onClick: () => askDelete(p), danger: true },
                    ]}
                  >
                    <MenuIcon />
                  </MenuButton>
                </td>
              </tr>
            ))}
            {shown.length === 0 && (
              <tr>
                <td colSpan={7} className="empty">
                  {search ? "No projects match the search." : "No projects yet. Create one to group keys by team, app or customer."}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {editing && (
        <ProjectForm
          initial={editing === "new" ? undefined : editing}
          confirm={setConfirming}
          onClose={() => setEditing(null)}
          onSaved={async () => {
            setEditing(null);
            await load();
          }}
        />
      )}
      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
    </>
  );
}

/** Creates a project, or edits `initial`; `onSaved` runs once it is saved. */
export function ProjectForm({
  initial,
  confirm,
  onSaved,
  onClose,
}: {
  initial?: Project;
  /** Asks, once saved, whether to delete what logging stored when logging was just denied. */
  confirm?: (r: ConfirmRequest) => void;
  onSaved: () => Promise<void>;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [period, setPeriod] = useState<BudgetPeriod | "">(initial?.budget_period ?? "");
  const [budget, setBudget] = useState(initial?.max_budget_usd?.toString() ?? "");
  const [guardrails, setGuardrails] = useState<Guardrail[]>([]);
  const [guardrailIds, setGuardrailIds] = useState<string[]>(initial?.guardrail_ids ?? []);
  const [webSearch, setWebSearch] = useState(initial?.web_search ?? true);
  const [promptLogging, setPromptLogging] = useState<PromptLogging>(initial?.prompt_logging ?? "deny");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Only admins edit projects, and they can list the guardrails to choose from.
  useEffect(() => {
    api<Guardrail[]>("/guardrails")
      .then(setGuardrails)
      .catch((e) => setError((e as Error).message));
  }, []);

  const amount = optNum(budget);
  const budgetMissing = period !== "" && amount == null;

  const submit = async () => {
    setBusy(true);
    try {
      const json: ProjectInput = {
        name,
        description: description.trim() || null,
        enabled: initial?.enabled ?? true,
        max_budget_usd: period ? amount : null,
        budget_period: period || null,
        guardrail_ids: guardrailIds,
        web_search: webSearch,
        prompt_logging: promptLogging,
      };
      if (initial) await api(`/projects/${initial.id}`, { method: "PUT", json });
      else await api("/projects", { method: "POST", json });
      await onSaved();
      if (initial && confirm && initial.prompt_logging !== "deny" && promptLogging === "deny") {
        await offerDeleteStoredPrompts(`/projects/${initial.id}`, name, confirm);
      }
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={initial ? "Edit project" : "New project"}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy || !name.trim() || budgetMissing} onClick={submit}>
            {initial ? "Save" : "Create project"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="form">
        <TextInput caption="Name" value={name} onChange={(e) => setName(e.target.value)} autoFocus placeholder="e.g. search-team" required />
        <TextArea caption="Description" rows={3} value={description} onChange={(e) => setDescription(e.target.value)} placeholder="optional" />
        <div className="row">
          <Select
            caption="Budget"
            hint="For all the project's keys together, on top of each key's own budget. Resets at the start of each UTC day, week (Monday) or month, independently of the keys' budgets."
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
            <TextInput caption="Amount (USD)" inputMode="decimal" value={budget} onChange={(e) => setBudget(e.target.value)} placeholder="required" required />
          ) : (
            <div />
          )}
        </div>
        {guardrails.length > 0 && (
          <MultiSelect
            caption="Guardrails"
            hint="Every request of every key in the project goes through these, whatever it asks for, on top of each key's own guardrails."
            values={guardrailIds}
            onChange={setGuardrailIds}
            none="None"
            options={guardrails.map((g) => ({ value: g.id, label: g.name, note: g.description }))}
          />
        )}
        <Toggle
          caption="Web search"
          hint="Whether the project's keys may have the model search the web, billed per search at the deployment's price. Off, no key in the project can turn it on."
          checked={webSearch}
          onChange={setWebSearch}
        />
        <Segmented
          caption="Prompt logging"
          hint="For debugging, not for production keys: whether full prompts, guardrail results and responses of the project's keys are stored, unencrypted. Deny stores none, whatever a key says. Allow stores those of the keys that opt in. Force stores every key's. It stays as set until it is changed."
          options={[
            { value: "deny", label: "Deny" },
            { value: "allow", label: "Allow" },
            { value: "force", label: "Force" },
          ]}
          value={promptLogging}
          onChange={setPromptLogging}
        />
        {promptLogging === "force" && initial?.prompt_logging !== "force" && (
          <Banner tone="warning">
            Every request of every key in this project will have its prompt and response stored, unencrypted, until this is changed. Use it for debugging, not for
            production traffic.
          </Banner>
        )}
      </div>
    </Modal>
  );
}
