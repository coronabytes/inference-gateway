import { useState } from "react";
import type { AliasInput, AutoRouter, Deployment, ModelAlias } from "../api";
import { Button, MenuButton, Select, TextInput, Toggle } from "../components/controls";
import type { SelectOption } from "../components/SearchSelect";
import { Badge, EditIcon, ErrorBanner, IconButton, MenuIcon, Modal } from "../components/ui";

/** A model an alias can point to, by name, with the id it is stored as. */
interface Target {
  name: string;
  /** A deployment's id (an enabled one where the model has any) or the router's. */
  id: string;
  /** The mode, or "router". */
  kind: string;
}

/** Every model name an alias can target: the deployments' and the routers'. */
function targets(deployments: Deployment[], routers: AutoRouter[]): Target[] {
  const byName = new Map<string, Target>();
  // An enabled deployment stands for its model name where there is one. The gateway keeps an
  // alias on the model when that deployment is renamed or deleted.
  for (const d of [...deployments].sort((a, b) => Number(b.enabled) - Number(a.enabled))) {
    if (!byName.has(d.model_name)) byName.set(d.model_name, { name: d.model_name, id: d.id, kind: d.mode });
  }
  for (const r of routers) byName.set(r.model_name, { name: r.model_name, id: r.id, kind: "router" });
  return [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
}

/** One alias: its name and the model it serves. */
export function AliasCard({
  alias,
  onEdit,
  onDuplicate,
  onToggle,
  onDelete,
}: {
  alias: ModelAlias;
  onEdit: () => void;
  onDuplicate: () => void;
  /** Activate or deactivate the alias. */
  onToggle: () => void;
  onDelete: () => void;
}) {
  return (
    <section className="card list-card alias-card">
      <header className="list-head">
        <div className="list-title">
          <h3 className="mono">
            {alias.model_name} <Badge tone="neutral">alias</Badge>
            {!alias.enabled && (
              <>
                {" "}
                <Badge tone="warning">Inactive</Badge>
              </>
            )}
          </h3>
          <p className="muted small" title="Requests for the alias are served by this model">
            serves{" "}
            {alias.target ? (
              <span className="mono">{alias.target}</span>
            ) : (
              <Badge tone="critical">missing</Badge>
            )}
          </p>
        </div>
        <div className="actions">
          <IconButton label={`Edit ${alias.model_name}`} title="Edit" onClick={onEdit}>
            <EditIcon />
          </IconButton>
          <MenuButton
            className="icon-btn"
            label={`More actions for ${alias.model_name}`}
            items={[
              { label: "Duplicate", onClick: onDuplicate },
              { label: alias.enabled ? "Deactivate" : "Activate", onClick: onToggle },
              { label: "Delete", onClick: onDelete, danger: true },
            ]}
          >
            <MenuIcon />
          </MenuButton>
        </div>
      </header>
    </section>
  );
}

const MODEL_NAME = /^[A-Za-z0-9\-_.:/@]{1,128}$/;

export function AliasForm({
  initial,
  copyOf,
  deployments,
  routers,
  taken,
  onSave,
  onDelete,
  onClose,
}: {
  initial?: ModelAlias;
  /** Duplicate: a new alias that starts with this one's values. */
  copyOf?: ModelAlias;
  deployments: Deployment[];
  routers: AutoRouter[];
  /** Other aliases' names, which this one cannot have. */
  taken: string[];
  onSave: (a: AliasInput) => Promise<void>;
  /** Offered when editing: closes the dialog and asks for confirmation. */
  onDelete?: () => void;
  onClose: () => void;
}) {
  const all = targets(deployments, routers);
  const from = initial ?? copyOf;
  const [name, setName] = useState(initial?.model_name ?? (copyOf ? `${copyOf.model_name}-copy` : ""));
  // Picked by model name; saved as the id the alias has, or else the one that stands for the name.
  const [target, setTarget] = useState(from?.target ?? "");
  const [enabled, setEnabled] = useState(from?.enabled ?? true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const trimmed = name.trim();
  const clash = all.some((t) => t.name === trimmed) ? "A deployment or router has this name." : taken.includes(trimmed) ? "Another alias has this name." : null;
  const missing = !!from && !from.target && !target;
  const invalid = !MODEL_NAME.test(trimmed) || !!clash || !target;
  const options: SelectOption[] = all.map((t) => ({ value: t.name, label: t.name, note: t.kind }));

  const submit = async () => {
    const picked = all.find((t) => t.name === target);
    if (!picked) return;
    setBusy(true);
    try {
      // Kept when the model is unchanged, so the alias stays on the deployment it follows.
      const target_id = from && from.target === target ? from.target_id : picked.id;
      await onSave({ model_name: trimmed, target_id, enabled });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={initial ? "Edit alias" : "Add alias"}
      onClose={onClose}
      footer={
        <>
          {onDelete && (
            <Button variant="danger" onClick={onDelete}>
              Delete
            </Button>
          )}
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" disabled={busy || invalid} onClick={submit}>
            {initial ? "Save" : "Add alias"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} />
      {!all.length && (
        <div className="banner">
          An alias serves an existing model. <a href="#/models/deployments">Add a deployment</a> first.
        </div>
      )}
      {missing && <div className="banner">The model this alias served was deleted. Pick another one to save it.</div>}
      <div className="form">
        <div className="row with-switch">
          <TextInput
            caption="Name"
            hint="The model name clients send as `model`. Keys that list allowed models need this name among them."
            mono
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. gpt-4"
            required
            autoFocus
            aria-invalid={clash ? true : undefined}
          />
          <Toggle caption="Active" checked={enabled} onChange={setEnabled} />
        </div>
        {clash && (
          <p className="small" role="alert">
            {clash}
          </p>
        )}
        <Select
          caption="Serves"
          hint="The model requests for the alias go to, with all its deployments. The alias keeps serving it when it is renamed."
          value={target}
          onChange={setTarget}
          required
          searchable
          empty="No models"
          options={options}
        />
      </div>
    </Modal>
  );
}
