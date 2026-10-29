import { useState } from "react";
import { adminFetch } from "../api";
import { labelHeaders, type Labels } from "../labels";
import { Button, Segmented, Select, TextArea, TextInput } from "../components/controls";
import { CurlButton } from "../components/CurlButton";
import { ErrorBanner, IconButton, TrashIcon } from "../components/ui";
import { compact, ms } from "../format";

interface Meta {
  model: string;
  latency: number;
  tokens?: number;
  requestId?: string;
}

/** POST to the admin playground as a key, with `labels`; returns the JSON body and timing. */
async function run(path: string, keyId: string, body: unknown, labels: Labels = {}): Promise<{ data: any; latency: number; requestId?: string }> {
  const started = performance.now();
  const res = await adminFetch(`/playground/${path}?key_id=${encodeURIComponent(keyId)}`, body, undefined, labelHeaders(labels));
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data?.error?.message ?? data?.error ?? `request failed (${res.status})`);
  return {
    data,
    latency: performance.now() - started,
    requestId: res.headers.get("x-request-id") ?? undefined,
  };
}

function MetaLine({ meta, extra }: { meta: Meta; extra?: string }) {
  return (
    <div className="muted small" title={meta.requestId ? `x-request-id: ${meta.requestId}` : undefined}>
      {meta.model} · {ms(meta.latency)}
      {meta.tokens != null && ` · ${compact(meta.tokens)} input tokens`}
      {extra && ` · ${extra}`}
    </div>
  );
}

function pct(p: number): string {
  return `${(p * 100).toFixed(p >= 0.995 || p < 0.005 ? 0 : 1)}%`;
}

function Bar({ label, p, strong }: { label: string; p: number; strong?: boolean }) {
  return (
    <div className={`prob-row${strong ? " strong" : ""}`}>
      <span className="prob-label" title={label}>
        {label}
      </span>
      <div className="meter" role="meter" aria-valuenow={Math.round(p * 100)} aria-valuemin={0} aria-valuemax={100} aria-label={label}>
        <div style={{ width: `${Math.min(1, Math.max(0, p)) * 100}%` }} />
      </div>
      <span className="prob-value">{pct(p)}</span>
    </div>
  );
}

// ---------------------------------------------------------------- embeddings

function cosine(a: number[], b: number[]): number {
  let dot = 0;
  let na = 0;
  let nb = 0;
  for (let i = 0; i < Math.min(a.length, b.length); i++) {
    dot += a[i] * b[i];
    na += a[i] * a[i];
    nb += b[i] * b[i];
  }
  return na && nb ? dot / Math.sqrt(na * nb) : 0;
}

/** What every panel gets from the Playground: the key it calls as, the model, and the labels to send. */
interface PanelProps {
  keyId: string;
  keyName?: string;
  model: string;
  labels: Labels;
}

export function EmbeddingPanel({ keyId, keyName, model, labels }: PanelProps) {
  const [text, setText] = useState("The export job hangs at 99%.\nExports never finish.\nHow do I change my password?");
  const [dimensions, setDimensions] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{
    inputs: string[];
    vectors: number[][];
    meta: Meta;
  } | null>(null);

  const inputs = text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

  const body = () => {
    const dims = parseInt(dimensions, 10);
    return {
      model,
      input: inputs.length === 1 ? inputs[0] : inputs,
      ...(dims > 0 && { dimensions: dims }),
    };
  };

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const { data, latency, requestId } = await run("embeddings", keyId, body(), labels);
      const vectors: number[][] = (data.data ?? []).sort((a: any, b: any) => a.index - b.index).map((d: any) => d.embedding);
      setResult({
        inputs,
        vectors,
        meta: {
          model: data.model ?? model,
          latency,
          tokens: data.usage?.prompt_tokens,
          requestId,
        },
      });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="panel-grid">
      <div className="card form">
        <TextArea caption="Inputs, one per line" rows={6} value={text} onChange={(e) => setText(e.target.value)} />
        <div className="row">
          <TextInput caption="Dimensions" inputMode="numeric" value={dimensions} onChange={(e) => setDimensions(e.target.value)} placeholder="model default" />
          <div className="panel-actions">
            <CurlButton request={() => ({ path: "/v1/embeddings", body: body(), labels })} keyName={keyName} disabled={!inputs.length} />
            <Button variant="primary" onClick={submit} disabled={busy || !inputs.length}>
              {busy ? "Embedding…" : "Embed"}
            </Button>
          </div>
        </div>
      </div>
      <div className="card">
        <ErrorBanner error={error} />
        {!result && !error && <div className="muted">Vectors and their cosine similarities appear here.</div>}
        {result && (
          <>
            <MetaLine meta={result.meta} extra={`${result.vectors.length} × ${result.vectors[0]?.length ?? 0} dimensions`} />
            <div className="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>Input</th>
                    <th className="num">Norm</th>
                    <th>First values</th>
                  </tr>
                </thead>
                <tbody>
                  {result.vectors.map((v, i) => (
                    <tr key={i}>
                      <td className="clip" title={result.inputs[i]}>
                        {result.inputs[i]}
                      </td>
                      <td className="num">{Math.sqrt(v.reduce((s, x) => s + x * x, 0)).toFixed(3)}</td>
                      <td className="mono small">
                        [
                        {v
                          .slice(0, 4)
                          .map((x) => x.toFixed(4))
                          .join(", ")}
                        {v.length > 4 ? ", …" : ""}]
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            {result.vectors.length > 1 && result.vectors.length <= 8 && (
              <>
                <h4>Cosine similarity</h4>
                <div className="table-wrap">
                  <table className="similarity">
                    <thead>
                      <tr>
                        <th />
                        {result.vectors.map((_, i) => (
                          <th key={i} className="num">
                            #{i + 1}
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {result.vectors.map((a, i) => (
                        <tr key={i}>
                          <th className="clip" title={result.inputs[i]}>
                            #{i + 1} {result.inputs[i]}
                          </th>
                          {result.vectors.map((b, j) => (
                            <td key={j} className={`num${i === j ? " muted" : ""}`}>
                              {cosine(a, b).toFixed(3)}
                            </td>
                          ))}
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </>
            )}
          </>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- rerank

export function RerankPanel({ keyId, keyName, model, labels }: PanelProps) {
  const [query, setQuery] = useState("Why does my export never finish?");
  const [text, setText] = useState(
    "Exports of more than 10,000 rows run in the background and are emailed when done.\nHow do I change my password?\nThe export job hangs at 99% when a column contains emoji.\nOur office is closed on public holidays.",
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{ documents: string[]; ranked: { index: number; score: number }[]; searches?: number; meta: Meta } | null>(null);

  const documents = text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const { data, latency, requestId } = await run("rerank", keyId, { model, query, documents }, labels);
      setResult({
        documents,
        ranked: (data.results ?? []).map((r: any) => ({ index: r.index, score: r.relevance_score })),
        searches: data.meta?.billed_units?.search_units,
        meta: { model: data.model ?? model, latency, tokens: data.meta?.billed_units?.input_tokens, requestId },
      });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="panel-grid">
      <div className="card form">
        <TextInput caption="Query" value={query} onChange={(e) => setQuery(e.target.value)} />
        <TextArea caption="Documents, one per line" rows={6} value={text} onChange={(e) => setText(e.target.value)} />
        <div className="panel-actions">
          <CurlButton
            request={() => ({ path: "/v1/rerank", body: { model, query, documents }, labels })}
            keyName={keyName}
            disabled={!query.trim() || !documents.length}
          />
          <Button variant="primary" onClick={submit} disabled={busy || !query.trim() || !documents.length}>
            {busy ? "Ranking…" : "Rerank"}
          </Button>
        </div>
      </div>
      <div className="card">
        <ErrorBanner error={error} />
        {!result && !error && <div className="muted">The documents, most relevant first, appear here.</div>}
        {result && (
          <>
            <MetaLine meta={result.meta} extra={result.searches != null ? `${result.searches} search${result.searches === 1 ? "" : "es"}` : undefined} />
            <div className="answers rerank-results">
              {result.ranked.map((r, i) => (
                <Bar key={r.index} label={`#${r.index + 1} ${result.documents[r.index]}`} p={r.score} strong={i === 0} />
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- decisions

type QType = "predicate" | "choice" | "score";

interface Q {
  type: QType;
  name: string;
  instructions: string;
  /** Choice values or score levels (lowest first), comma-separated. */
  options: string;
}

type DecisionApi = "decisions" | "systemone";

const API_LABEL: Record<DecisionApi, string> = {
  decisions: "Decisions (OpenAI)",
  systemone: "System One (TypeSafe)",
};

const QTYPE_LABEL: Record<QType, string> = {
  predicate: "Yes / no",
  choice: "Choice",
  score: "Score",
};

const EXAMPLE: Q[] = [
  {
    type: "predicate",
    name: "defect",
    instructions: "Is this a defect?",
    options: "",
  },
  {
    type: "choice",
    name: "severity",
    instructions: "How severe is it?",
    options: "low, medium, high",
  },
  {
    type: "score",
    name: "urgency",
    instructions: "How urgent is it?",
    options: "can wait, soon, now",
  },
];

const optionList = (q: Q) =>
  q.options
    .split(",")
    .map((o) => o.trim())
    .filter(Boolean);

function buildDecisionBody(apiId: DecisionApi, model: string, input: string, questions: Q[]) {
  if (apiId === "decisions") {
    return {
      model,
      input,
      questions: questions.map((q) => ({
        type: q.type,
        ...(q.name.trim() && { name: q.name.trim() }),
        instructions: q.instructions,
        ...(q.type === "choice" && {
          choices: optionList(q).map((value) => ({ value })),
        }),
        ...(q.type === "score" && {
          levels: optionList(q).map((label) => ({ label })),
        }),
      })),
    };
  }
  return {
    model,
    state: input,
    questions: Object.fromEntries(
      questions.map((q, i) => [
        q.name.trim() || `q${i + 1}`,
        {
          type: q.type === "predicate" ? "noul" : q.type,
          instructions: q.instructions,
          ...(q.type === "choice" && {
            criteria: Object.fromEntries(optionList(q).map((o) => [o, null])),
          }),
          ...(q.type === "score" && { criteria: optionList(q) }),
        },
      ]),
    ),
  };
}

/** One answer in a common shape, from either response format. */
interface Shown {
  title: string;
  refused?: boolean;
  headline?: string;
  bars: { label: string; p: number; strong?: boolean }[];
}

function showAnswers(apiId: DecisionApi, questions: Q[], data: any): Shown[] {
  if (apiId === "decisions") {
    return (data.answers ?? []).map((a: any, i: number): Shown => {
      const title = a.name ?? questions[i]?.name ?? `#${i + 1}`;
      switch (a.type) {
        case "predicate":
          return {
            title,
            headline: `yes ${pct(a.probability)}`,
            bars: [{ label: "yes", p: a.probability, strong: a.probability >= 0.5 }],
          };
        case "choice":
          return {
            title,
            headline: `${String(a.choice)} · confidence ${pct(a.confidence)}`,
            bars: a.probabilities.map((p: any) => ({
              label: String(p.value),
              p: p.probability,
              strong: p.value === a.choice,
            })),
          };
        case "score":
          return {
            title,
            headline: `score ${Number(a.score).toFixed(2)} · confidence ${pct(a.confidence)}`,
            bars: a.probabilities.map((p: any) => ({
              label: p.label,
              p: p.probability,
              strong: p.value === Math.round(a.score),
            })),
          };
        default:
          return { title, refused: true, bars: [] };
      }
    });
  }
  return Object.entries(buildDecisionBody("systemone", "", "", questions).questions).map(([key]): Shown => {
    const a = data.answers?.[key];
    if (!a) return { title: key, refused: true, bars: [] };
    switch (a.type) {
      case "noul":
        return {
          title: key,
          headline: `noul ${pct(a.noul)}`,
          bars: [{ label: "true", p: a.noul, strong: a.noul >= 0.5 }],
        };
      case "choice":
        return {
          title: key,
          headline: `${a.choice} · confidence ${pct(a.confidence)}`,
          bars: Object.entries(a.probabilities as Record<string, number>).map(([label, p]) => ({ label, p, strong: label === a.choice })),
        };
      case "score":
        return {
          title: key,
          headline: `score ${Number(a.score).toFixed(2)} · confidence ${pct(a.confidence)}`,
          bars: Object.entries(a.probabilities as Record<string, number>).map(([i, p]) => ({
            label: String(a.legend?.[i] ?? i),
            p,
            strong: Number(i) === Math.round(a.score),
          })),
        };
      default:
        return { title: key, refused: true, bars: [] };
    }
  });
}

export function DecisionPanel({ keyId, keyName, model, labels }: PanelProps) {
  const [apiId, setApiId] = useState<DecisionApi>("decisions");
  const [input, setInput] = useState("Ticket (billing): The export job hangs at 99% and never finishes.");
  const [questions, setQuestions] = useState<Q[]>(EXAMPLE);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{
    apiId: DecisionApi;
    asked: Q[];
    data: any;
    meta: Meta;
  } | null>(null);

  const patch = (i: number, p: Partial<Q>) => setQuestions((qs) => qs.map((q, j) => (j === i ? { ...q, ...p } : q)));
  const minOptions = apiId === "decisions" ? 2 : 1;
  const incomplete =
    !input.trim() ||
    !questions.length ||
    questions.some((q) => !q.instructions.trim() || (q.type !== "predicate" && optionList(q).length < minOptions));

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const { data, latency, requestId } = await run(apiId, keyId, buildDecisionBody(apiId, model, input, questions), labels);
      setResult({
        apiId,
        asked: questions,
        data,
        meta: {
          model: data.model ?? model,
          latency,
          tokens: data.usage?.input_tokens,
          requestId,
        },
      });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="panel-grid">
      <div className="card form">
        <Segmented
          caption="Format"
          label="Request format"
          options={(["decisions", "systemone"] as const).map((a) => ({ value: a, label: API_LABEL[a] }))}
          value={apiId}
          onChange={setApiId}
        />
        <TextArea
          caption={apiId === "decisions" ? "Input" : "State"}
          rows={4}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="The item to judge: a ticket, a document, a transcript…"
        />
        <div className="questions">
          {questions.map((q, i) => (
            <div key={i} className="question-row">
              <Select
                caption="Type"
                label={`Question ${i + 1} type`}
                value={q.type}
                onChange={(type) => patch(i, { type: type as QType })}
                options={(Object.keys(QTYPE_LABEL) as QType[]).map((t) => ({ value: t, label: QTYPE_LABEL[t] }))}
              />
              <TextInput
                caption="Name"
                mono
                value={q.name}
                onChange={(e) => patch(i, { name: e.target.value })}
                placeholder="e.g. urgent"
                aria-label={`Question ${i + 1} name`}
              />
              <TextInput
                caption="Question"
                value={q.instructions}
                onChange={(e) => patch(i, { instructions: e.target.value })}
                aria-label={`Question ${i + 1}`}
              />
              <TextInput
                caption={q.type === "score" ? "Levels" : "Options"}
                value={q.options}
                onChange={(e) => patch(i, { options: e.target.value })}
                placeholder={q.type === "choice" ? "comma-separated" : q.type === "score" ? "lowest first" : "—"}
                disabled={q.type === "predicate"}
                aria-label={`Question ${i + 1} options`}
              />
              <IconButton
                label={`Remove question ${i + 1}`}
                title="Remove"
                onClick={() => setQuestions((qs) => qs.filter((_, j) => j !== i))}
              >
                <TrashIcon />
              </IconButton>
            </div>
          ))}
        </div>
        <div className="panel-actions">
          <Button
            variant="ghost"
            onClick={() => setQuestions((qs) => [...qs, { type: "predicate", name: "", instructions: "", options: "" }])}
          >
            Add question
          </Button>
          <CurlButton
            request={() => ({ path: `/v1/${apiId}`, body: buildDecisionBody(apiId, model, input, questions), labels })}
            keyName={keyName}
            disabled={incomplete}
          />
          <Button variant="primary" onClick={submit} disabled={busy || incomplete}>
            {busy ? "Deciding…" : "Decide"}
          </Button>
        </div>
      </div>
      <div className="card">
        <ErrorBanner error={error} />
        {!result && !error && <div className="muted">Answers appear here, with the probability of every option.</div>}
        {result && (
          <>
            <MetaLine meta={result.meta} />
            <div className="answers">
              {showAnswers(result.apiId, result.asked, result.data).map((a, i) => (
                <div key={i} className="answer">
                  <div className="answer-head">
                    <strong className="mono">{a.title}</strong>
                    <span className="muted small">{a.refused ? "refused" : a.headline}</span>
                  </div>
                  {a.bars.map((b, j) => (
                    <Bar key={j} label={b.label} p={b.p} strong={b.strong} />
                  ))}
                </div>
              ))}
            </div>
            <details>
              <summary className="muted small">Raw response</summary>
              <pre className="raw">{JSON.stringify(result.data, null, 2)}</pre>
            </details>
          </>
        )}
      </div>
    </div>
  );
}
