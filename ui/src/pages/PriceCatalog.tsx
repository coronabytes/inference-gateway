import { Fragment, useCallback, useEffect, useState } from "react";
import { api, type ImportSummary, type PriceEntry, type PriceImport, type PriceSearch } from "../api";
import { Banner, Button, Select, TextInput } from "../components/controls";
import { ErrorBanner } from "../components/ui";
import { compact, relative, until } from "../format";
import { catalogPrices, perMillion } from "../pricing";

const PAGE = 100;

function importSummary(i: PriceImport): string {
  return `${compact(i.total)} entries (${i.added} new, ${i.updated} changed, ${i.removed} removed)`;
}

/** LiteLLM's price catalog as a filterable grid, with the import button. */
export function PriceCatalog() {
  const [q, setQ] = useState("");
  const [provider, setProvider] = useState("");
  const [mode, setMode] = useState("");
  const [data, setData] = useState<PriceSearch | null>(null);
  const [items, setItems] = useState<PriceEntry[]>([]);
  const [open, setOpen] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [importing, setImporting] = useState(false);

  const fetchPage = useCallback(
    async (offset: number) => {
      const params = new URLSearchParams({ q, limit: String(PAGE), offset: String(offset) });
      if (provider) params.set("provider", provider);
      if (mode) params.set("mode", mode);
      return api<PriceSearch>(`/prices?${params}`);
    },
    [q, provider, mode],
  );

  // Debounced, so typing in the search box does not send a request per key.
  useEffect(() => {
    const t = setTimeout(async () => {
      try {
        const r = await fetchPage(0);
        setData(r);
        setItems(r.items);
        setError(null);
      } catch (e) {
        setError((e as Error).message);
      }
    }, 200);
    return () => clearTimeout(t);
  }, [fetchPage]);

  const more = async () => {
    try {
      const r = await fetchPage(items.length);
      setItems((cur) => [...cur, ...r.items]);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const runImport = async () => {
    setImporting(true);
    setNotice(null);
    try {
      const r = await api<ImportSummary>("/prices/import", { method: "POST" });
      const accepted = r.accepted === undefined ? "" : ` ${r.accepted} deployment${r.accepted === 1 ? "" : "s"} took the new prices.`;
      setNotice(`Imported ${importSummary(r)}.${accepted}`);
      const page = await fetchPage(0);
      setData(page);
      setItems(page.items);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setImporting(false);
    }
  };

  const empty = data && !data.last_import;
  return (
    <>
      <div className="page-head">
        <div className="filters">
          <TextInput caption="Model" type="search" value={q} onChange={(e) => setQ(e.target.value)} placeholder="search" />
          <Select
            caption="Provider"
            value={provider}
            onChange={setProvider}
            options={[{ value: "", label: "All" }, ...(data?.providers ?? []).map((p) => ({ value: p, label: p }))]}
          />
          <Select
            caption="Mode"
            value={mode}
            onChange={setMode}
            options={[{ value: "", label: "All" }, ...(data?.modes ?? []).map((m) => ({ value: m, label: m }))]}
          />
          {data && !empty && <span className="muted small">{compact(data.total)} entries</span>}
        </div>
        <div className="page-actions">
          {data?.last_import && (
            <span className="muted small" title={`${new Date(data.last_import.imported_at).toLocaleString()}: ${importSummary(data.last_import)}`}>
              Last import {relative(data.last_import.imported_at)}
            </span>
          )}
          {data && (
            <span
              className="muted small"
              title={
                data.next_import
                  ? `${new Date(data.next_import).toLocaleString()}. Change the schedule under Cluster → Settings.`
                  : "Turn it on under Cluster → Settings."
              }
            >
              {data.next_import ? `next ${until(data.next_import)}` : "scheduled import off"}
            </span>
          )}
          <Button variant="primary" onClick={runImport} disabled={importing} title="Imports LiteLLM's model prices.">
            {importing ? "Importing…" : "Run Import"}
          </Button>
        </div>
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {notice && (
        <Banner tone="good" caption="Import" onDismiss={() => setNotice(null)}>
          {notice}
        </Banner>
      )}
      {empty ? (
        <div className="empty-card">
          The price catalog is empty. <strong>Run Import</strong> downloads LiteLLM's model prices (about 4,000 models) so new deployments can
          fill in their prices.{data.next_import && " The scheduled import also does this within a few minutes."}
        </div>
      ) : (
        <div className="table-wrap">
          <table className="catalog-table">
            <colgroup>
              <col />
              <col className="col-provider" />
              <col className="col-mode" />
              <col className="col-price" span={4} />
              <col className="col-tokens" span={2} />
            </colgroup>
            <thead>
              <tr>
                <th>Model</th>
                <th>Provider</th>
                <th>Mode</th>
                <th className="num">Input</th>
                <th className="num">Output</th>
                <th className="num">Cache read</th>
                <th className="num">Cache write</th>
                <th className="num">Context</th>
                <th className="num">Max output</th>
              </tr>
            </thead>
            <tbody>
              {items.map((it) => {
                const p = catalogPrices(it.data);
                const expanded = open === it.key;
                return (
                  <Fragment key={it.key}>
                    <tr className="clickable" onClick={() => setOpen(expanded ? null : it.key)} aria-expanded={expanded}>
                      <td className="mono clip" title={it.key}>{it.key}</td>
                      <td className="clip" title={p.provider ?? undefined}>{p.provider ?? "–"}</td>
                      <td className="clip" title={p.mode ?? undefined}>{p.mode ?? "–"}</td>
                      <td className="num">{perMillion(p.input)}</td>
                      <td className="num">{perMillion(p.output)}</td>
                      <td className="num">{perMillion(p.cacheRead)}</td>
                      <td className="num">{perMillion(p.cacheWrite)}</td>
                      <td className="num">{p.maxInput ? compact(p.maxInput) : "–"}</td>
                      <td className="num">{p.maxOutput ? compact(p.maxOutput) : "–"}</td>
                    </tr>
                    {expanded && (
                      <tr className="child-row">
                        <td colSpan={9}>
                          {p.long && (
                            <p className="small">
                              Above {compact(p.long.threshold)} input tokens: input {perMillion(p.long.input)}, output {perMillion(p.long.output)}
                              {p.long.cacheRead !== null && `, cache read ${perMillion(p.long.cacheRead)}`}
                              {p.long.cacheWrite !== null && `, cache write ${perMillion(p.long.cacheWrite)}`}.
                            </p>
                          )}
                          {p.perKWebSearches !== null && (
                            <p className="small">Web search: ${p.perKWebSearches} per 1,000 searches, filled into chat deployments of this model.</p>
                          )}
                          <pre className="raw">{JSON.stringify(it.data, null, 2)}</pre>
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
              {items.length === 0 && data && (
                <tr>
                  <td colSpan={9} className="empty">No catalog entry matches.</td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      )}
      {data && items.length < data.total && (
        <div className="panel-actions">
          <button type="button" onClick={more}>Show more ({compact(data.total - items.length)} left)</button>
        </div>
      )}
    </>
  );
}
