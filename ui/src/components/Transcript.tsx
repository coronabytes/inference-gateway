import { useEffect, useState } from "react";
import { api, type GuardrailStep, type LogBodies } from "../api";
import { compact, usd } from "../format";
import { Button, Segmented } from "./controls";
import { ErrorBanner } from "./ui";

/** One part of a turn, as shown: text, a tool call or result, reasoning, or anything else as JSON. */
type Part =
  | { kind: "text"; text: string }
  | { kind: "reasoning"; text: string }
  | { kind: "tool_call"; name: string; args: string }
  | { kind: "tool_result"; text: string; error?: boolean }
  | { kind: "search"; query: string; results: { url: string; title?: string }[]; label?: string }
  | { kind: "json"; label: string; value: unknown };

type Turn = { role: string; parts: Part[] };

type Obj = Record<string, unknown>;
const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);
const str = (v: unknown) => (typeof v === "string" ? v : v == null ? "" : JSON.stringify(v, null, 2));

/** A placeholder for what is never stored (an image, a file, an encrypted blob). */
function omitted(v: unknown): string | null {
  return isObj(v) && "x_gateway_omitted" in v ? `[${String(v.x_gateway_omitted)}, ${compact(Number(v.bytes ?? 0))} bytes, not stored]` : null;
}

/** The parts of a message's content, in any of the three APIs' shapes. */
function parts(content: unknown): Part[] {
  if (content == null) return [];
  if (typeof content === "string") return [{ kind: "text", text: content }];
  if (!Array.isArray(content)) return [{ kind: "json", label: "content", value: content }];
  return content.flatMap((p): Part[] => {
    if (!isObj(p)) return [{ kind: "text", text: str(p) }];
    const type = String(p.type ?? "");
    if (typeof p.text === "string" && ["text", "input_text", "output_text", "summary_text"].includes(type)) return [{ kind: "text", text: p.text }];
    if (type === "thinking") return [{ kind: "reasoning", text: str(p.thinking) }];
    if (type === "tool_use" || type === "server_tool_use") return [{ kind: "tool_call", name: String(p.name), args: str(p.input) }];
    if (type === "tool_result") return [{ kind: "tool_result", text: parts(p.content).map((x) => ("text" in x ? x.text : str(x))).join("\n"), error: !!p.is_error }];
    if (type === "web_search_tool_result" && Array.isArray(p.content)) {
      return [{ kind: "search", query: "", results: p.content.filter(isObj).map((r) => ({ url: String(r.url ?? ""), title: r.title as string | undefined })) }];
    }
    for (const k of ["image_url", "source", "file", "file_data", "image", "input_audio"]) {
      const o = omitted(isObj(p[k]) && "url" in (p[k] as Obj) ? (p[k] as Obj).url : isObj(p[k]) && "data" in (p[k] as Obj) ? (p[k] as Obj).data : p[k]);
      if (o) return [{ kind: "text", text: o }];
    }
    return [{ kind: "json", label: type || "part", value: p }];
  });
}

/** The conversation a request holds, by role, for chat, messages and responses requests. */
function requestTurns(apiName: string, req: unknown): Turn[] | null {
  if (!isObj(req)) return null;
  const turns: Turn[] = [];
  if (apiName === "messages" && req.system != null) turns.push({ role: "system", parts: parts(req.system) });
  if (apiName === "responses" && req.instructions != null) turns.push({ role: "system", parts: parts(req.instructions) });
  if (apiName === "chat" || apiName === "messages") {
    if (!Array.isArray(req.messages)) return null;
    for (const m of req.messages.filter(isObj)) {
      const ps = parts(m.content);
      if (Array.isArray(m.tool_calls)) {
        for (const c of m.tool_calls.filter(isObj)) {
          const f = isObj(c.function) ? c.function : {};
          ps.push({ kind: "tool_call", name: String(f.name ?? ""), args: str(f.arguments) });
        }
      }
      turns.push({ role: m.role === "tool" ? "tool" : String(m.role ?? ""), parts: m.role === "tool" ? [{ kind: "tool_result", text: ps.map((p) => ("text" in p ? p.text : "")).join("\n") }] : ps });
    }
    return turns;
  }
  if (apiName === "responses") {
    if (typeof req.input === "string") return [...turns, { role: "user", parts: [{ kind: "text", text: req.input }] }];
    if (!Array.isArray(req.input)) return null;
    for (const it of req.input.filter(isObj)) {
      if (it.type === "function_call") turns.push({ role: "assistant", parts: [{ kind: "tool_call", name: String(it.name), args: str(it.arguments) }] });
      else if (it.type === "function_call_output") turns.push({ role: "tool", parts: [{ kind: "tool_result", text: str(it.output) }] });
      else turns.push({ role: String(it.role ?? it.type ?? ""), parts: parts(it.content) });
    }
    return turns;
  }
  return null;
}

/** What the model answered, for chat, messages and responses responses. */
function responseTurns(apiName: string, res: unknown): Turn[] | null {
  if (!isObj(res)) return null;
  if (apiName === "chat" && Array.isArray(res.choices)) {
    return res.choices.filter(isObj).map((c) => {
      const m = isObj(c.message) ? c.message : {};
      const ps: Part[] = [];
      if (m.reasoning_content) ps.push({ kind: "reasoning", text: str(m.reasoning_content) });
      ps.push(...parts(m.content));
      if (m.refusal) ps.push({ kind: "text", text: `Refused: ${str(m.refusal)}` });
      for (const t of Array.isArray(m.tool_calls) ? m.tool_calls.filter(isObj) : []) {
        const f = isObj(t.function) ? t.function : {};
        ps.push({ kind: "tool_call", name: String(f.name ?? ""), args: str(f.arguments) });
      }
      const cites = Array.isArray(m.annotations) ? m.annotations.filter(isObj) : [];
      if (cites.length) {
        ps.push({ kind: "search", label: "Cited", query: "", results: cites.map((a) => (isObj(a.url_citation) ? a.url_citation : a)).map((u) => ({ url: String(u.url ?? ""), title: u.title as string | undefined })) });
      }
      return { role: "assistant", parts: ps };
    });
  }
  if (apiName === "messages" && Array.isArray(res.content)) return [{ role: "assistant", parts: parts(res.content) }];
  if (apiName === "responses" && Array.isArray(res.output)) {
    const ps: Part[] = [];
    for (const it of res.output.filter(isObj)) {
      if (it.type === "message") ps.push(...parts(it.content));
      else if (it.type === "reasoning") ps.push({ kind: "reasoning", text: parts(it.summary).map((p) => ("text" in p ? p.text : "")).join("\n") });
      else if (it.type === "function_call") ps.push({ kind: "tool_call", name: String(it.name), args: str(it.arguments) });
      else if (it.type === "web_search_call") {
        const action = isObj(it.action) ? it.action : {};
        const sources = Array.isArray(action.sources) ? action.sources.filter(isObj) : [];
        ps.push({ kind: "search", query: str(action.query), results: sources.map((s) => ({ url: String(s.url ?? "") })) });
      } else ps.push({ kind: "json", label: String(it.type ?? "item"), value: it });
    }
    return [{ role: "assistant", parts: ps }];
  }
  return null;
}

/**
 * What the provider ran itself and cited on a translated request, which the client's API had no
 * place for (`x_gateway_not_sent`): web searches with their queries and results, and citations.
 */
function notSent(res: unknown): Part[] | null {
  if (!isObj(res) || !isObj(res.x_gateway_not_sent)) return null;
  const { server_tools, annotations } = res.x_gateway_not_sent as { server_tools?: unknown[]; annotations?: unknown[] };
  const out: Part[] = [];
  for (const b of (server_tools ?? []).filter(isObj)) {
    const type = String(b.type ?? "");
    const input = isObj(b.input) ? b.input : {};
    const action = isObj(b.action) ? b.action : {};
    const google = isObj(b.google) ? b.google : {};
    if (type === "server_tool_use") out.push({ kind: "search", query: str(input.query ?? input), results: [] });
    else if (type.endsWith("_tool_result") && Array.isArray(b.content)) {
      out.push({ kind: "search", query: "", results: b.content.filter(isObj).map((r) => ({ url: String(r.url ?? ""), title: r.title as string | undefined })) });
    } else if (type === "web_search_call") {
      const sources = Array.isArray(action.sources) ? action.sources.filter(isObj) : [];
      out.push({ kind: "search", query: str(action.query), results: sources.map((s) => ({ url: String(s.url ?? "") })) });
    } else if (type === "google_search" && Array.isArray(google.web_search_queries)) {
      out.push({ kind: "search", query: google.web_search_queries.map(String).join(" · "), results: [] });
    } else out.push({ kind: "json", label: type || "server tool", value: b });
  }
  const cites = (annotations ?? []).filter(isObj).map((a) => (isObj(a.url_citation) ? a.url_citation : a));
  if (cites.length) out.push({ kind: "search", label: "Cited", query: "", results: cites.map((c) => ({ url: String(c.url ?? ""), title: c.title as string | undefined })) });
  return out;
}

function PartView({ p }: { p: Part }) {
  switch (p.kind) {
    case "text":
      return <div className="transcript-text">{p.text}</div>;
    case "reasoning":
      return (
        <details className="transcript-reasoning">
          <summary className="muted small">Reasoning</summary>
          <div className="transcript-text">{p.text}</div>
        </details>
      );
    case "tool_call":
      return (
        <div className="transcript-tool">
          <span className="muted small">Calls</span> <span className="mono">{p.name}</span>
          <pre className="raw wrap">{p.args}</pre>
        </div>
      );
    case "tool_result":
      return (
        <div className="transcript-tool">
          <span className="muted small">{p.error ? "Tool error" : "Tool result"}</span>
          <pre className="raw wrap">{p.text}</pre>
        </div>
      );
    case "search":
      return (
        <div className="transcript-tool">
          <span className="muted small">{p.label ?? "Web search"}</span> {p.query && <span className="mono">{p.query}</span>}
          <ul className="transcript-sources">
            {p.results.map((r, i) => (
              <li key={i} className="small">
                {r.title ? `${r.title} · ` : ""}
                <span className="mono">{r.url}</span>
              </li>
            ))}
          </ul>
        </div>
      );
    case "json":
      return (
        <details className="transcript-reasoning">
          <summary className="muted small">{p.label}</summary>
          <pre className="raw">{JSON.stringify(p.value, null, 2)}</pre>
        </details>
      );
  }
}

/** A document as a conversation where its API is known, else as JSON. */
function Document({ turns, value }: { turns: Turn[] | null; value: unknown }) {
  if (!turns) return <pre className="raw">{JSON.stringify(value, null, 2)}</pre>;
  return (
    <ol className="transcript">
      {turns.map((t, i) => (
        <li key={i} className={`transcript-turn role-${t.role}`}>
          <div className="transcript-role">{t.role}</div>
          <div>
            {t.parts.map((p, j) => (
              <PartView key={j} p={p} />
            ))}
          </div>
        </li>
      ))}
    </ol>
  );
}

function GuardrailSteps({ steps }: { steps: GuardrailStep[] }) {
  return (
    <ol className="transcript">
      {steps.map((s, i) => (
        <li key={i} className="transcript-turn">
          <div className="transcript-role">{s.guardrail}</div>
          <div>
            <div className="small">
              <span className={`guardrail-mark ${s.action}`}>{s.action}</span>
              {s.duration_ms != null && <span className="muted">{s.duration_ms} ms</span>}
              {s.cost_usd != null && <span className="muted"> · {usd(s.cost_usd)}</span>}
              {s.error && <span> · {s.error}</span>}
            </div>
            {s.message && <div className="transcript-text">{s.message}</div>}
            {s.findings && s.findings.length > 0 && (
              <table className="detail-table">
                <tbody>
                  {s.findings.map((f, j) => (
                    <tr key={j}>
                      <th>
                        {f.policy}:{f.kind}
                      </th>
                      <td>{f.action}</td>
                      <td className="mono">{f.matched ?? "–"}</td>
                      <td className="muted">{f.confidence ?? ""}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
            {s.changes?.map((c, j) => (
              <div key={j} className="transcript-change">
                <del>{c.before}</del>
                <ins>{c.after}</ins>
              </div>
            ))}
            {s.provider_response != null && (
              <details className="transcript-reasoning">
                <summary className="muted small">Provider's answer</summary>
                <pre className="raw">{JSON.stringify(s.provider_response, null, 2)}</pre>
              </details>
            )}
          </div>
        </li>
      ))}
    </ol>
  );
}

type View = "sent" | "screened" | "json";

/**
 * A logged request's prompt and response (prompt logging), loaded when shown, with the turns
 * before and after it in its conversation.
 */
export function Bodies({ id, api: apiName }: { id: string; api: string }) {
  const [current, setCurrent] = useState(id);
  const [data, setData] = useState<LogBodies | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [view, setView] = useState<View>("sent");
  const [raw, setRaw] = useState(false);

  useEffect(() => {
    let live = true;
    setData(null);
    api<LogBodies>(`/logs/${current}/bodies`)
      .then((d) => live && (setData(d), setError(null)))
      .catch((e) => live && setError((e as Error).message));
    return () => {
      live = false;
    };
  }, [current]);

  if (error) return <ErrorBanner error={error} onDismiss={() => setError(null)} />;
  if (!data) return <p className="muted small">Loading…</p>;

  const request = view === "screened" ? data.screened_request : data.request;
  const options = [
    { value: "sent" as View, label: "As sent" },
    ...(data.screened_request ? [{ value: "screened" as View, label: "As the model got it" }] : []),
    { value: "json" as View, label: "JSON" },
  ];
  return (
    <div className="bodies">
      <section>
        <Segmented
          caption="Request"
          hint={`${compact(data.request_bytes)} bytes as sent. Images, files and encrypted data are not stored${data.status === "truncated" ? "; long texts were cut to the size limit" : ""}.`}
          options={options}
          value={view}
          onChange={setView}
        />
        {view === "json" ? <pre className="raw">{JSON.stringify(data.request, null, 2)}</pre> : <Document turns={requestTurns(apiName, request)} value={request} />}
      </section>
      {data.guardrails && data.guardrails.length > 0 && (
        <section>
          <h4>Guardrails</h4>
          <GuardrailSteps steps={data.guardrails} />
        </section>
      )}
      <section>
        <Segmented
          caption="Response"
          hint={`${compact(data.response_bytes)} bytes as received.`}
          options={[
            { value: "turns", label: "Answer" },
            { value: "json", label: "JSON" },
          ]}
          value={raw ? "json" : "turns"}
          onChange={(v) => setRaw(v === "json")}
        />
        {data.response == null ? (
          <p className="muted small">No response: the request was refused or failed before one came.</p>
        ) : raw ? (
          <pre className="raw">{JSON.stringify(data.response, null, 2)}</pre>
        ) : (
          <Document turns={responseTurns(apiName, data.response)} value={data.response} />
        )}
        {!raw && notSent(data.response) && (
          <div className="bodies-not-sent">
            <h4>From the provider, not sent to the client</h4>
            <p className="muted small">The provider searched the web or cited sources; translated to the client's API, the answer had no place for these.</p>
            <Document turns={[{ role: "provider", parts: notSent(data.response)! }]} value={null} />
          </div>
        )}
        {isObj(data.response) && data.response.x_gateway_incomplete === true && (
          <p className="muted small">The response stopped before it was complete: the client went away or the upstream failed.</p>
        )}
      </section>
      <div className="bodies-actions">
        {data.previous && (
          <Button variant="ghost" onClick={() => setCurrent(data.previous!)}>
            Previous turn
          </Button>
        )}
        {data.next && (
          <Button variant="ghost" onClick={() => setCurrent(data.next!)}>
            Next turn
          </Button>
        )}
      </div>
    </div>
  );
}
