import { Fragment, useEffect, useMemo, useState } from "react";
import { api } from "../api";
import { Banner, Button, FileButton, Toggle } from "../components/controls";
import { ErrorBanner } from "../components/ui";

interface Changes {
  created: string[];
  updated: string[];
  /** Entries of the file that match one here that is already the same. */
  unchanged: number;
}

type SectionKey =
  | "credentials"
  | "guardrails"
  | "projects"
  | "deployments"
  | "routers"
  | "aliases"
  | "model_metadata"
  | "mcp_servers"
  | "a2a_agents"
  | "tool_routers"
  | "keys";

type ImportResult = Record<SectionKey, Changes> & {
  dry_run: boolean;
  /** New credentials, MCP servers and A2A agents that work once their secrets are entered. */
  needs_secrets: string[];
  mcp_servers_need_secrets: string[];
  a2a_agents_need_secrets: string[];
};

/** The document's sections, in the order they are imported; `needs` must come along (references by name). */
const SECTIONS: { key: SectionKey; label: string; needs?: SectionKey[]; note?: string }[] = [
  { key: "credentials", label: "Credentials", note: "without their secrets" },
  { key: "guardrails", label: "Guardrails", needs: ["credentials"] },
  { key: "projects", label: "Projects" },
  { key: "deployments", label: "Deployments", needs: ["credentials"] },
  { key: "routers", label: "Auto routers", needs: ["deployments"] },
  { key: "aliases", label: "Model aliases", needs: ["deployments"] },
  { key: "model_metadata", label: "Model descriptions", needs: ["deployments"] },
  { key: "mcp_servers", label: "MCP servers", note: "without their secrets" },
  { key: "a2a_agents", label: "A2A agents", note: "without their secrets" },
  { key: "tool_routers", label: "Tool routers", needs: ["mcp_servers", "deployments"] },
  { key: "keys", label: "Virtual keys", needs: ["projects"], note: "with the hash of their secret" },
];

/** The sections that go along: those chosen, and those they need, where `available` has them. */
function effective(chosen: Set<SectionKey>, available: (k: SectionKey) => boolean): Set<SectionKey> {
  const out = new Set([...chosen].filter(available));
  for (let grew = true; grew; ) {
    grew = false;
    for (const s of SECTIONS) {
      if (!out.has(s.key)) continue;
      for (const n of s.needs ?? []) {
        if (available(n) && !out.has(n)) {
          out.add(n);
          grew = true;
        }
      }
    }
  }
  return out;
}

/** The chosen sections that need `k`, by label, while it is chosen with them. */
function neededBy(k: SectionKey, on: Set<SectionKey>): string[] {
  return SECTIONS.filter((s) => on.has(s.key) && s.needs?.includes(k)).map((s) => s.label);
}

/** A switch per section; one another chosen section needs stays on, and says which. */
function SectionPicker({
  chosen,
  on,
  onChange,
  counts,
}: {
  chosen: Set<SectionKey>;
  /** What goes along: the chosen sections and what they need. */
  on: Set<SectionKey>;
  onChange: (chosen: Set<SectionKey>) => void;
  /** For an import: how many entries the file has of each; sections it lacks are not offered. */
  counts?: Partial<Record<SectionKey, number>>;
}) {
  return (
    <div className="transfer-sections" role="group" aria-label="Sections">
      {SECTIONS.filter((s) => !counts || counts[s.key]).map((s) => {
        const by = chosen.has(s.key) ? [] : neededBy(s.key, on);
        const hint = by.length ? `Needed by ${by.join(" and ")}: they name it.` : s.note ? `${s.label}, ${s.note}.` : undefined;
        return (
          <Toggle
            key={s.key}
            caption={counts ? `${s.label} (${counts[s.key]})` : s.label}
            hint={hint}
            checked={on.has(s.key)}
            disabled={by.length > 0}
            onChange={(v: boolean) => {
              const next = new Set(chosen);
              if (v) next.add(s.key);
              else next.delete(s.key);
              onChange(next);
            }}
          />
        );
      })}
    </div>
  );
}

/** How many entries of each section a document has. */
function countSections(doc: unknown): Partial<Record<SectionKey, number>> {
  const out: Partial<Record<SectionKey, number>> = {};
  if (!doc || typeof doc !== "object") return out;
  for (const s of SECTIONS) {
    const v = (doc as Record<string, unknown>)[s.key];
    if (Array.isArray(v) && v.length) out[s.key] = v.length;
  }
  return out;
}

/** The document with only the sections in `on`. */
function only(doc: unknown, on: Set<SectionKey>): unknown {
  const copy = { ...(doc as Record<string, unknown>) };
  for (const s of SECTIONS) if (!on.has(s.key)) delete copy[s.key];
  return copy;
}

const nothingChanges = (r: ImportResult, on: Iterable<SectionKey>) => [...on].every((k) => !r[k] || r[k].created.length + r[k].updated.length === 0);

/** Per section: how many entries the file has, and how many are new, updated and unchanged; the
 *  names unfold under each row. */
function Summary({ result, counts, on }: { result: ImportResult; counts: Partial<Record<SectionKey, number>>; on: Set<SectionKey> }) {
  const [open, setOpen] = useState<SectionKey | null>(null);
  // Keys follow renamed models even when the file has none of them.
  const rows = SECTIONS.filter((s) => on.has(s.key) || (result[s.key] && result[s.key].created.length + result[s.key].updated.length > 0));
  return (
    <div className="table-wrap">
      <table className="transfer-table">
        <thead>
          <tr>
            <th>Section</th>
            <th className="num">In file</th>
            <th className="num">New</th>
            <th className="num">Updated</th>
            <th className="num">Unchanged</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((s) => {
            const c = result[s.key] ?? { created: [], updated: [], unchanged: 0 };
            const names = c.created.length + c.updated.length > 0;
            return (
              <Fragment key={s.key}>
                <tr>
                  <td>
                    {names ? (
                      <button type="button" className="transfer-unfold" aria-expanded={open === s.key} onClick={() => setOpen(open === s.key ? null : s.key)}>
                        {s.label}
                      </button>
                    ) : (
                      s.label
                    )}
                  </td>
                  <td className="num">{counts[s.key] ?? 0}</td>
                  <td className="num">{c.created.length || ""}</td>
                  <td className="num">{c.updated.length || ""}</td>
                  <td className="num muted">{c.unchanged || ""}</td>
                </tr>
                {open === s.key && (
                  <tr className="transfer-names">
                    <td colSpan={5}>
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
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

/** What needs a secret entered after an import, with where to enter it. */
function Secrets({ result }: { result: ImportResult }) {
  const parts = [
    { names: result.needs_secrets, what: "credentials", href: "#/models/credentials", page: "Credentials" },
    { names: result.mcp_servers_need_secrets, what: "MCP servers", href: "#/tools/mcp", page: "MCP servers" },
    { names: result.a2a_agents_need_secrets, what: "A2A agents", href: "#/tools/agents", page: "A2A agents" },
  ].filter((p) => p.names?.length);
  if (!parts.length) return null;
  return (
    <ul className="transfer-secrets small">
      {parts.map((p) => (
        <li key={p.what}>
          New {p.what} without secrets: {p.names.join(", ")}. Enter them on the <a href={p.href}>{p.page}</a> page; until then they do not work.
        </li>
      ))}
    </ul>
  );
}

/** Settings: export chosen sections of the configuration as JSON, or import chosen sections of a file (checked first, then applied). */
export function ConfigTransfer() {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [exportChosen, setExportChosen] = useState<Set<SectionKey>>(() => new Set(SECTIONS.map((s) => s.key)));
  const exportOn = useMemo(() => effective(exportChosen, () => true), [exportChosen]);
  const [file, setFile] = useState<{ name: string; doc: unknown; counts: Partial<Record<SectionKey, number>> } | null>(null);
  const [importChosen, setImportChosen] = useState<Set<SectionKey>>(new Set());
  const importOn = useMemo(() => effective(importChosen, (k) => !!file?.counts[k]), [importChosen, file]);
  const [preview, setPreview] = useState<ImportResult | null>(null);
  const [done, setDone] = useState<{ result: ImportResult; counts: Partial<Record<SectionKey, number>>; on: Set<SectionKey> } | null>(null);
  const reset = () => {
    setFile(null);
    setPreview(null);
  };

  const exportConfig = async () => {
    setBusy(true);
    try {
      const sections = SECTIONS.filter((s) => exportOn.has(s.key)).map((s) => s.key);
      const doc = await api<unknown>(`/config/export?sections=${sections.join(",")}`);
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

  // Choosing a file offers its sections, every one on.
  const choose = async (f: File | undefined) => {
    if (!f) return;
    setDone(null);
    setPreview(null);
    try {
      let doc: unknown;
      try {
        doc = JSON.parse(await f.text());
      } catch {
        throw new Error(`${f.name} is not a JSON file.`);
      }
      const counts = countSections(doc);
      if (!Object.keys(counts).length) throw new Error(`${f.name} has nothing to import.`);
      setFile({ name: f.name, doc, counts });
      setImportChosen(new Set(Object.keys(counts) as SectionKey[]));
      setError(null);
    } catch (e) {
      setFile(null);
      setError((e as Error).message);
    }
  };

  // The chosen sections are checked against this gateway: what the import would change.
  useEffect(() => {
    if (!file) return;
    if (!importOn.size) return setPreview(null);
    let current = true;
    setBusy(true);
    api<ImportResult>("/config/import?dry_run=true", { method: "POST", json: only(file.doc, importOn) })
      .then((r) => {
        if (!current) return;
        setPreview(r);
        setError(null);
      })
      .catch((e) => {
        if (!current) return;
        setPreview(null);
        setError((e as Error).message);
      })
      .finally(() => current && setBusy(false));
    return () => {
      current = false;
    };
  }, [file, importOn]);

  const apply = async () => {
    if (!file) return;
    setBusy(true);
    try {
      const result = await api<ImportResult>("/config/import", { method: "POST", json: only(file.doc, importOn) });
      setDone({ result, counts: file.counts, on: importOn });
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
        The configuration as one JSON file, to back it up or move it to another gateway. Secrets are never exported: after an import, enter those of new
        credentials, MCP servers and A2A agents again. Keys come with the hash of their secret, so imported keys work with the same secret; an existing key
        keeps its own. An import updates what has the same id, or else what matches by name, and adds the rest; it never deletes anything. Renames carry
        over: routers, keys and descriptions of a renamed model follow the new name.
      </p>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />

      <h4 className="transfer-head">Export</h4>
      <SectionPicker chosen={exportChosen} on={exportOn} onChange={setExportChosen} />
      <div className="transfer-actions">
        <Button onClick={exportConfig} disabled={busy || !exportOn.size}>
          Export {exportOn.size === SECTIONS.length ? "configuration" : `${exportOn.size} section${exportOn.size === 1 ? "" : "s"}`}
        </Button>
        {exportOn.has("keys") && <span className="muted small">The file names your keys and their limits: keep it safe.</span>}
      </div>

      <h4 className="transfer-head">Import</h4>
      <div className="transfer-actions">
        <FileButton accept=".json,application/json" onFile={choose} disabled={busy}>
          {file ? "Choose another file…" : "Import from file…"}
        </FileButton>
        {file && <span className="small">{file.name}</span>}
      </div>

      {file && (
        <div className="transfer-preview">
          <SectionPicker chosen={importChosen} on={importOn} onChange={setImportChosen} counts={file.counts} />
          {preview && importOn.size > 0 && (
            <>
              {nothingChanges(preview, importOn) && <p className="muted">Nothing to change: this gateway already matches these sections of the file.</p>}
              <Summary result={preview} counts={file.counts} on={importOn} />
              <Secrets result={preview} />
            </>
          )}
          <div className="modal-foot">
            <Button onClick={reset} disabled={busy}>
              Cancel
            </Button>
            <Button variant="primary" onClick={apply} disabled={busy || !preview || !importOn.size || nothingChanges(preview, importOn)}>
              Import {importOn.size} section{importOn.size === 1 ? "" : "s"}
            </Button>
          </div>
        </div>
      )}

      {done && (
        <Banner tone="good" caption="Imported" onDismiss={() => setDone(null)}>
          <Summary result={done.result} counts={done.counts} on={done.on} />
          <Secrets result={done.result} />
        </Banner>
      )}
    </section>
  );
}
