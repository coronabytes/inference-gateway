import { useState } from "react";
import { api } from "../api";
import { Button, FileButton } from "../components/controls";
import { ErrorBanner } from "../components/ui";

interface Changes {
  created: string[];
  updated: string[];
}

interface ImportResult {
  dry_run: boolean;
  credentials: Changes;
  guardrails: Changes;
  deployments: Changes;
  routers: Changes;
  aliases: Changes;
  /** Keys whose allowed models follow a renamed model. */
  keys: Changes;
  /** Models whose display name and description the file sets, or that follow a rename. */
  model_metadata: Changes;
  /** New credentials whose secrets must be entered before their deployments work. */
  needs_secrets: string[];
}

const KINDS = [
  { key: "credentials", label: "Credentials" },
  { key: "guardrails", label: "Guardrails" },
  { key: "deployments", label: "Deployments" },
  { key: "routers", label: "Auto routers" },
  { key: "aliases", label: "Model aliases" },
  { key: "keys", label: "Keys" },
  { key: "model_metadata", label: "Model descriptions" },
] as const;

function nothingChanges(r: ImportResult): boolean {
  return KINDS.every(({ key }) => r[key].created.length + r[key].updated.length === 0);
}

/** What an import creates and updates, per kind; the names are listed when expanded. */
function Summary({ result }: { result: ImportResult }) {
  if (nothingChanges(result)) return <p className="muted">Nothing to change: this gateway already matches the file.</p>;
  return (
    <ul className="transfer-summary">
      {KINDS.map(({ key, label }) => {
        const c = result[key];
        if (!c.created.length && !c.updated.length) return null;
        return (
          <li key={key}>
            <details>
              <summary>
                <strong>{label}</strong>: {[c.created.length && `${c.created.length} new`, c.updated.length && `${c.updated.length} updated`].filter(Boolean).join(", ")}
              </summary>
              <ul>
                {c.created.map((n) => (
                  <li key={`n-${n}`}>
                    <span className="mono">{n}</span> <span className="muted small">new</span>
                  </li>
                ))}
                {c.updated.map((n) => (
                  <li key={`u-${n}`}>
                    <span className="mono">{n}</span> <span className="muted small">updated</span>
                  </li>
                ))}
              </ul>
            </details>
          </li>
        );
      })}
    </ul>
  );
}

/** Settings: export the configuration as JSON, or import one (checked first, then applied). */
export function ConfigTransfer() {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [file, setFile] = useState<{ name: string; doc: unknown } | null>(null);
  const [preview, setPreview] = useState<ImportResult | null>(null);
  const [done, setDone] = useState<ImportResult | null>(null);
  const reset = () => {
    setFile(null);
    setPreview(null);
  };

  const exportConfig = async () => {
    setBusy(true);
    try {
      const doc = await api<unknown>("/config/export");
      const blob = new Blob([JSON.stringify(doc, null, 2)], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `inference-gateway-config-${new Date().toISOString().slice(0, 10)}.json`;
      a.click();
      URL.revokeObjectURL(url);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  // Choosing a file only checks it: the gateway reports what the import would change.
  const choose = async (f: File | undefined) => {
    if (!f) return;
    setDone(null);
    setPreview(null);
    setBusy(true);
    try {
      let doc: unknown;
      try {
        doc = JSON.parse(await f.text());
      } catch {
        throw new Error(`${f.name} is not a JSON file.`);
      }
      setFile({ name: f.name, doc });
      setPreview(await api<ImportResult>("/config/import?dry_run=true", { method: "POST", json: doc }));
      setError(null);
    } catch (e) {
      setFile(null);
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const apply = async () => {
    if (!file) return;
    setBusy(true);
    try {
      setDone(await api<ImportResult>("/config/import", { method: "POST", json: file.doc }));
      reset();
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card form">
      <h3>Backup &amp; Restore</h3>
      <p className="muted small">
        Credentials, guardrails, models, auto routers, model aliases and the models' names and descriptions from the Hub as one JSON file, to back them up or move them to another gateway. Credential secrets are never
        exported: after an import, enter them again on the Models page's Credentials tab. An import updates what has the same id, or else what
        matches by name (deployments by model, credential and upstream model), and adds the rest; it never deletes anything. Keys and projects are not
        in the file; the guardrails set on them stay as they are. Renames carry over:
        routers, keys and descriptions of a renamed model follow the new name, and aliases follow the model they point to.
      </p>
      <ErrorBanner error={error} />
      <div className="transfer-actions">
        <Button onClick={exportConfig} disabled={busy}>
          Export configuration
        </Button>
        <FileButton accept=".json,application/json" onFile={choose} disabled={busy}>
          Import from file…
        </FileButton>
      </div>

      {file && preview && (
        <div className="transfer-preview">
          <p>
            <strong>{file.name}</strong> would change:
          </p>
          <Summary result={preview} />
          {preview.needs_secrets.length > 0 && (
            <p className="muted small">
              New credentials without secrets: {preview.needs_secrets.join(", ")}. Their deployments work once the secrets are entered.
            </p>
          )}
          <div className="modal-foot">
            <button onClick={reset} disabled={busy}>
              Cancel
            </button>
            <button className="primary" onClick={apply} disabled={busy || nothingChanges(preview)}>
              Import
            </button>
          </div>
        </div>
      )}

      {done && (
        <div className="banner good" role="status">
          <p>Imported.</p>
          <Summary result={done} />
          {done.needs_secrets.length > 0 && (
            <p>
              Enter the secrets of {done.needs_secrets.join(", ")} on the <a href="#/models/credentials">Credentials</a> tab.
            </p>
          )}
        </div>
      )}
    </section>
  );
}
