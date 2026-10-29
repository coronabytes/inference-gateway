import { useState } from "react";
import type { AutoRouter, Deployment, ReasoningEffort, Route, RouterInput } from "../api";
import { Button, MenuButton, MultiSelect, Select, TextArea, TextInput, Toggle } from "../components/controls";
import { Badge, EditIcon, ErrorBanner, IconButton, MenuIcon, Modal, TrashIcon } from "../components/ui";
import { TagEditor, TagList } from "../components/Tags";
import { effortOptions, supportedEfforts } from "../efforts";

/** Model names of a mode among the deployments. */
function modelNames(deployments: Deployment[], mode: Deployment["mode"]): string[] {
  return [...new Set(deployments.filter((d) => d.mode === mode).map((d) => d.model_name))].sort();
}

/** Marks a model the router names that no deployment serves any more (e.g. after a rename). */
function Missing({ show }: { show: boolean }) {
  return show ? (
    <>
      {" "}
      <Badge tone="critical">missing</Badge>
    </>
  ) : null;
}

/** Names offered for new routes, in order, like complexity tiers. */
const ROUTE_NAMES = ["Simple", "Medium", "Complex", "Reasoning"];

/** One auto router: its decision model and the routes it chooses from. */
export function RouterCard({
  router,
  deployments,
  onEdit,
  onDuplicate,
  onToggle,
  onDelete,
}: {
  router: AutoRouter;
  deployments: Deployment[];
  onEdit: () => void;
  onDuplicate: () => void;
  /** Activate or deactivate the router. */
  onToggle: () => void;
  onDelete: () => void;
}) {
  const fallback = router.fallback_route ?? router.routes[0]?.name;
  const chat = modelNames(deployments, "chat");
  const decision = modelNames(deployments, "decision");
  return (
    <section className="card list-card router-card">
      <header className="list-head">
        <div className="list-title">
          <h3 className="mono">
            {router.model_name} <Badge tone="neutral">router</Badge>
            {!router.enabled && (
              <>
                {" "}
                <Badge tone="warning">Inactive</Badge>
              </>
            )}
            {router.tags.length > 0 && (
              <>
                {" "}
                <TagList tags={router.tags} />
              </>
            )}
          </h3>
          <p className="muted small" title={router.instructions ? `Decision model, asked: “${router.instructions}”` : "Decision model"}>
            <span className="mono">{router.decision_model}</span>
            <Missing show={!decision.includes(router.decision_model)} />
          </p>
        </div>
        <div className="actions">
          <IconButton label={`Edit ${router.model_name}`} title="Edit" onClick={onEdit}>
            <EditIcon />
          </IconButton>
          <MenuButton
            className="icon-btn"
            label={`More actions for ${router.model_name}`}
            items={[
              { label: "Duplicate", onClick: onDuplicate },
              { label: router.enabled ? "Deactivate" : "Activate", onClick: onToggle },
              { label: "Delete", onClick: onDelete, danger: true },
            ]}
          >
            <MenuIcon />
          </MenuButton>
        </div>
      </header>
      {/* Each route: its name, its models, and when the decision model should pick it. */}
      <ul className="list">
        {router.routes.map((r) => (
          <li key={r.name} className="list-item">
            <div className="stat">
              <span className="route-name">{r.name}</span>
              {r.name === fallback && <span className="muted small">fallback</span>}
            </div>
            <ul className="route-models">
              {r.models.map((m) => (
                <li key={m.model}>
                  <span className="mono">{m.model}</span>
                  <Missing show={!chat.includes(m.model)} />
                  {m.reasoning_effort && <span className="muted small"> {m.reasoning_effort} effort</span>}
                </li>
              ))}
            </ul>
            <div className="muted">{r.description}</div>
          </li>
        ))}
      </ul>
    </section>
  );
}

const MODEL_NAME = /^[A-Za-z0-9\-_.:/@]{1,128}$/;

export function RouterForm({
  initial,
  copyOf,
  deployments,
  knownTags,
  onSave,
  onDelete,
  onClose,
}: {
  initial?: AutoRouter;
  /** Duplicate: a new router that starts with this one's values. */
  copyOf?: AutoRouter;
  deployments: Deployment[];
  /** Tags deployments and routers use, offered while typing. */
  knownTags: string[];
  onSave: (r: RouterInput) => Promise<void>;
  /** Offered when editing: closes the dialog and asks for confirmation. */
  onDelete?: () => void;
  onClose: () => void;
}) {
  const chatModels = modelNames(deployments, "chat");
  const decisionModels = modelNames(deployments, "decision");
  // An edited router, or one being duplicated: its values start the form. A copy needs its own name.
  const from = initial ?? copyOf;
  const [name, setName] = useState(initial?.model_name ?? (copyOf ? `${copyOf.model_name}-copy` : ""));
  const [decisionModel, setDecisionModel] = useState(from?.decision_model ?? decisionModels[0] ?? "");
  const [instructions, setInstructions] = useState(from?.instructions ?? "");
  const [routes, setRoutes] = useState<Route[]>(
    from?.routes ?? [{ name: ROUTE_NAMES[0], description: "", models: chatModels.length ? [{ model: chatModels[0] }] : [] }],
  );
  const [fallback, setFallback] = useState(from?.fallback_route ?? "");
  const [tags, setTags] = useState<string[]>(from?.tags ?? []);
  const [enabled, setEnabled] = useState(from?.enabled ?? true);
  const [sticky, setSticky] = useState(from?.sticky ?? true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const patch = (i: number, p: Partial<Route>) => setRoutes((rs) => rs.map((r, j) => (j === i ? { ...r, ...p } : r)));
  /** Keeps each model's effort when the selection changes; new models start with the client's. */
  const pickModels = (i: number, names: string[]) =>
    patch(i, { models: names.map((model) => routes[i].models.find((m) => m.model === model) ?? { model }) });
  const setEffort = (i: number, model: string, effort: string) =>
    patch(i, { models: routes[i].models.map((m) => (m.model === model ? { model, reasoning_effort: (effort || null) as ReasoningEffort | null } : m)) });
  const addRoute = () => {
    const taken = new Set(routes.map((r) => r.name.trim().toLowerCase()));
    const next = ROUTE_NAMES.find((n) => !taken.has(n.toLowerCase())) ?? `Route ${routes.length + 1}`;
    setRoutes((rs) => [...rs, { name: next, description: "", models: [] }]);
  };

  const names = routes.map((r) => r.name.trim().toLowerCase());
  const duplicateName = new Set(names).size !== names.length;
  // Names a select holds but no deployment serves any more, e.g. after a model was renamed.
  const decisionMissing = !!decisionModel && !decisionModels.includes(decisionModel);
  const missing = decisionMissing || routes.some((r) => r.models.some((m) => !chatModels.includes(m.model)));
  // An effort the model does not take, e.g. saved before its catalog entry said so.
  const unsupported = (m: Route["models"][number]) => !!m.reasoning_effort && !supportedEfforts(deployments, m.model).includes(m.reasoning_effort);
  const badEffort = routes.some((r) => r.models.some(unsupported));
  const invalid =
    !MODEL_NAME.test(name.trim()) ||
    !decisionModel ||
    !routes.length ||
    duplicateName ||
    missing ||
    badEffort ||
    routes.some((r) => !r.name.trim() || !r.description.trim() || !r.models.length);
  const label = (m: string, known: string[]) => (known.includes(m) ? m : `${m} (missing)`);

  const submit = async () => {
    setBusy(true);
    try {
      await onSave({
        model_name: name.trim(),
        decision_model: decisionModel,
        instructions: instructions.trim() || null,
        routes: routes.map((r) => ({
          name: r.name.trim(),
          description: r.description.trim(),
          models: r.models.map((m) => (m.reasoning_effort ? { model: m.model, reasoning_effort: m.reasoning_effort } : { model: m.model })),
        })),
        fallback_route: fallback && routes.some((r) => r.name.trim() === fallback) ? fallback : null,
        tags,
        enabled,
        sticky,
      });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={initial ? "Edit router" : "Add router"}
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
            {initial ? "Save" : "Add router"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} />
      {(!decisionModels.length || !chatModels.length) && (
        <div className="banner">
          A router needs a decision model and at least one chat model. <a href="#/models/deployments">Add deployments</a> for them first.
        </div>
      )}
      {missing && (
        <div className="banner">
          This router names models that no longer exist (marked missing), for example after a rename. Pick current models to save it.
        </div>
      )}
      <div className="form">
        <div className="row with-switch">
          <TextInput
            caption="Name"
            hint="The model name clients send as `model`. Served on the chat endpoints."
            mono
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. auto"
            required
            autoFocus
          />
          <Select
            caption="Decision model"
            hint="Picks the route for each request."
            value={decisionModel}
            required
            onChange={setDecisionModel}
            invalid={decisionMissing}
            empty="No decision models"
            options={[
              ...(decisionMissing ? [{ value: decisionModel, label: label(decisionModel, decisionModels) }] : []),
              ...decisionModels.map((m) => ({ value: m, label: m })),
            ]}
          />
          <Toggle caption="Active" checked={enabled} onChange={setEnabled} />
        </div>
        <TextArea
          caption="Question (optional)"
          hint="Leave empty for: “Which route should handle this request? Pick the route whose description fits the request best.”"
          rows={2}
          value={instructions}
          onChange={(e) => setInstructions(e.target.value)}
        />

        <fieldset>
          <legend>Routes</legend>
          <div className="routes-editor">
            {routes.map((r, i) => {
              const options = [
                ...r.models.filter((m) => !chatModels.includes(m.model)).map((m) => ({ value: m.model, label: label(m.model, chatModels) })),
                ...chatModels.map((m) => ({ value: m, label: m })),
              ];
              const nameTaken = !!r.name.trim() && names.indexOf(r.name.trim().toLowerCase()) !== i;
              return (
                <div key={i} className="route-row">
                  <TextInput
                    caption="Name"
                    hint="What the decision model chooses, e.g. Simple or Complex. Unique within the router."
                    value={r.name}
                    onChange={(e) => patch(i, { name: e.target.value })}
                    aria-label={`Route ${i + 1} name`}
                    aria-invalid={nameTaken || undefined}
                    required
                  />
                  <MultiSelect
                    caption="Models"
                    label={`Route ${i + 1} models`}
                    hint="Each request on this route goes to one of these models at random."
                    none="None"
                    values={r.models.map((m) => m.model)}
                    onChange={(v) => pickModels(i, v)}
                    required
                    empty="No chat models"
                    options={options}
                  />
                  <IconButton label={`Remove route ${i + 1}`} title="Remove" onClick={() => setRoutes((rs) => rs.filter((_, j) => j !== i))}>
                    <TrashIcon />
                  </IconButton>
                  <TextArea
                    className="route-wide"
                    caption="When to use it"
                    rows={2}
                    value={r.description}
                    onChange={(e) => patch(i, { description: e.target.value })}
                    placeholder="e.g. “Hard reasoning, code and math. Slow and expensive.”"
                    aria-label={`Route ${i + 1} description`}
                    required
                  />
                  {r.models.length > 0 && (
                    <div className="route-wide route-efforts">
                      {r.models.map((m) => {
                        const supported = supportedEfforts(deployments, m.model);
                        return (
                          <Select
                            key={m.model}
                            caption={m.model}
                            label={`Reasoning effort for ${m.model} on route ${i + 1}`}
                            hint="Reasoning effort for this model on this route. Default keeps what the client asks for; any other level replaces it. Levels the model does not take, by its price catalog entry, cannot be chosen."
                            value={m.reasoning_effort ?? ""}
                            onChange={(v) => setEffort(i, m.model, v)}
                            searchable={false}
                            invalid={unsupported(m)}
                            options={effortOptions(supported, "Default effort")}
                          />
                        );
                      })}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
          {duplicateName && <p className="small" role="alert">Each route needs its own name.</p>}
          {badEffort && (
            <p className="small" role="alert">
              A model is set to a reasoning effort it does not take; pick another level for it.
            </p>
          )}
          <Button variant="ghost" disabled={!chatModels.length || routes.length >= 32} onClick={addRoute}>
            Add route
          </Button>
        </fieldset>

        <div className="row">
          <Select
            caption="Fallback"
            hint="The route used when the decision model fails or picks nothing usable."
            value={fallback}
            onChange={setFallback}
            options={[
              { value: "", label: `First route${routes[0]?.name.trim() ? ` (${routes[0].name.trim()})` : ""}` },
              ...routes.filter((r) => r.name.trim()).map((r) => ({ value: r.name.trim(), label: r.name.trim() })),
            ]}
          />
          <Toggle
            caption="Sticky conversations"
            hint="Keeps a conversation on the route and model its first message was sent to, for an hour after its last request. Its prompt cache stays warm and later messages need no decision. Off, the decision model picks a route for every message."
            checked={sticky}
            onChange={setSticky}
          />
        </div>
        <TagEditor
          caption="Tags"
          hint={
            <ul className="hint-list">
              <li>
                <strong>Keys with tags</strong> can call this router only if they hold every one of its tags, and then reach only the routes and models their tags allow.
              </li>
              <li>
                <strong>Keys without tags</strong> can call every router.
              </li>
            </ul>
          }
          value={tags}
          onChange={setTags}
          suggestions={knownTags}
        />
      </div>
    </Modal>
  );
}
