import { useEffect, useState } from "react";
import { oneOf, useSessionState } from "../sessionState";
import { adminFetch, type Voice } from "../api";
import { ANY_VOICE } from "../voices";
import {
  isSilent,
  MAX_SPEECH_CHARS,
  microphoneError,
  SILENT_RECORDING,
  speak,
  transcribe,
  useLiveTranscription,
  usePlayer,
  useRecorder,
  type Speech,
  type Transcript,
} from "../audio";
import { labelHeaders, type Labels } from "../labels";
import { Banner, Button, FileButton, Segmented, Select, TextArea, TextInput } from "../components/controls";
import { CurlButton } from "../components/CurlButton";
import { ErrorBanner, IconButton, MicIcon, StopIcon, TrashIcon } from "../components/ui";
import { bytes, compact, ms } from "../format";

interface Meta {
  model: string;
  latency: number;
  tokens?: number;
  requestId?: string;
}

/** POST to the admin playground as a key, with `labels`; returns the JSON body and timing. */
async function run(
  path: string,
  keyId: string,
  body: unknown,
  labels: Labels = {},
): Promise<{ data: any; latency: number; requestId?: string }> {
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
  const [text, setText] = useSessionState("playground.embeddings.text", "The export job hangs at 99%.\nExports never finish.\nHow do I change my password?");
  const [dimensions, setDimensions] = useSessionState("playground.embeddings.dimensions", "");
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
          <TextInput
            caption="Dimensions"
            inputMode="numeric"
            value={dimensions}
            onChange={(e) => setDimensions(e.target.value)}
            placeholder="model default"
          />
          <div className="panel-actions">
            <CurlButton request={() => ({ path: "/v1/embeddings", body: body(), labels })} keyName={keyName} disabled={!inputs.length} />
            <Button variant="primary" onClick={submit} disabled={busy || !inputs.length}>
              {busy ? "Embedding…" : "Embed"}
            </Button>
          </div>
        </div>
      </div>
      <div className="card">
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
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
  const [query, setQuery] = useSessionState("playground.rerank.query", "Why does my export never finish?");
  const [text, setText] = useSessionState(
    "playground.rerank.documents",
    "Exports of more than 10,000 rows run in the background and are emailed when done.\nHow do I change my password?\nThe export job hangs at 99% when a column contains emoji.\nOur office is closed on public holidays.",
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{
    documents: string[];
    ranked: { index: number; score: number }[];
    searches?: number;
    meta: Meta;
  } | null>(null);

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
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        {!result && !error && <div className="muted">The documents, most relevant first, appear here.</div>}
        {result && (
          <>
            <MetaLine
              meta={result.meta}
              extra={result.searches != null ? `${result.searches} search${result.searches === 1 ? "" : "es"}` : undefined}
            />
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
  const [apiId, setApiId] = useSessionState<DecisionApi>("playground.decisions.api", "decisions", oneOf(["decisions", "systemone"]));
  const [input, setInput] = useSessionState("playground.decisions.input", "Ticket (billing): The export job hangs at 99% and never finishes.");
  const [questions, setQuestions] = useSessionState<Q[]>("playground.decisions.questions", EXAMPLE);
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
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
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

// ---------------------------------------------------------------- speech to text

const TRANSCRIPT_FORMATS = [
  { value: "json", label: "JSON" },
  { value: "text", label: "Text" },
  { value: "verbose_json", label: "Segments" },
  { value: "srt", label: "SRT" },
];

export function TranscriptionPanel({ keyId, model, labels }: PanelProps) {
  const recorder = useRecorder();
  const [audio, setAudio] = useState<Blob | null>(null);
  const [language, setLanguage] = useSessionState("playground.transcription.language", "");
  const [format, setFormat] = useSessionState("playground.transcription.format", "json");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{ transcript: Transcript; meta: Meta } | null>(null);
  const url = useObjectUrl(audio);
  // A silent recording or file transcribes to nothing: said before it is sent.
  const [silent, setSilent] = useState(false);
  useEffect(() => {
    setSilent(false);
    if (!audio) return;
    let live = true;
    isSilent(audio).then((s) => live && setSilent(s));
    return () => {
      live = false;
    };
  }, [audio]);

  const record = async () => {
    setError(null);
    try {
      if (recorder.recording) setAudio(await recorder.stop());
      else await recorder.start();
    } catch (e) {
      setError(microphoneError(e));
    }
  };

  const submit = async () => {
    if (!audio) return;
    setBusy(true);
    setError(null);
    try {
      const t = await transcribe(keyId, model, audio, labels, { language: language.trim(), response_format: format });
      setResult({ transcript: t, meta: { model, latency: t.latency, requestId: t.requestId } });
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const usage = result?.transcript.usage;
  return (
    <div className="panel-grid">
      <div className="card form">
        <div className="audio-source">
          <Button onClick={record} disabled={!recorder.supported} aria-pressed={recorder.recording}>
            {recorder.recording ? <StopIcon /> : <MicIcon />} {recorder.recording ? "Stop recording" : "Record"}
          </Button>
          <FileButton label="Choose an audio file" accept="audio/*,video/mp4,video/webm" onFile={setAudio}>
            Choose a file
          </FileButton>
          {audio && (
            <span className="muted small">
              {audio instanceof File ? audio.name : "Recording"} · {bytes(audio.size)}
            </span>
          )}
        </div>
        {url && <audio className="audio-preview" controls src={url} />}
        {silent && <Banner tone="warning">{SILENT_RECORDING}</Banner>}
        <div className="row">
          <TextInput
            caption="Language"
            hint="The audio's language as an ISO-639-1 code, e.g. en or de. Optional; it improves accuracy."
            value={language}
            onChange={(e) => setLanguage(e.target.value)}
            placeholder="detect"
          />
          <Select caption="Format" value={format} onChange={setFormat} options={TRANSCRIPT_FORMATS} />
        </div>
        <div className="panel-actions">
          <Button variant="primary" onClick={submit} disabled={busy || !audio || recorder.recording}>
            {busy ? "Transcribing…" : "Transcribe"}
          </Button>
        </div>
      </div>
      <div className="card">
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        {!result && !error && <div className="muted">Record or choose audio, and its transcript appears here.</div>}
        {result && (
          <>
            <MetaLine
              meta={result.meta}
              extra={
                usage?.type === "duration"
                  ? `${usage.seconds} s of audio`
                  : usage?.type === "tokens"
                    ? `${compact(usage.input_tokens ?? 0)} in / ${compact(usage.output_tokens ?? 0)} out`
                    : undefined
              }
            />
            <pre className="transcript">{format === "json" || format === "text" ? result.transcript.text : result.transcript.raw}</pre>
          </>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- text to speech

const SPEECH_FORMATS = [
  { value: "pcm", label: "pcm", note: "streamed" },
  ...["mp3", "wav", "opus", "flac"].map((f) => ({ value: f, label: f })),
];

export function SpeechPanel({ keyId, keyName, model, labels, voices }: PanelProps & { voices: Voice[] }) {
  const [text, setText] = useSessionState("playground.speech.text", "Hello! This is the inference gateway, reading text aloud.");
  // The voices the model's deployments list: a choice among them, and any other one where `*` lets it through.
  const listed = voices.filter((v) => v.id !== ANY_VOICE);
  const open = !listed.length || voices.some((v) => v.id === ANY_VOICE);
  const [choice, setChoice] = useState(listed[0]?.id ?? ANY_VOICE);
  const [typed, setTyped] = useState("");
  const voice = choice === ANY_VOICE ? typed : choice;
  const [format, setFormat] = useSessionState("playground.speech.format", "pcm", oneOf(SPEECH_FORMATS.map((f) => f.value)));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{ audio: Blob; meta: Meta; characters: number; streamed: boolean } | null>(null);
  const player = usePlayer();
  const url = useObjectUrl(result?.audio ?? null);
  const body = { model, input: text, ...(voice.trim() && { voice: voice.trim() }), response_format: format };

  const submit = async () => {
    setBusy(true);
    setError(null);
    const done = (s: Speech, streamed: boolean) =>
      setResult({ audio: s.audio, characters: [...text].length, streamed, meta: { model, latency: s.latency, requestId: s.requestId } });
    try {
      if (format === "pcm") {
        // Plays while it arrives; the audio shows here once all of it has.
        await player.play("speech", async (p, signal) => {
          done(await speak(keyId, model, text, labels, { voice: voice.trim() }, signal, p), true);
          setBusy(false);
        });
      } else {
        done(await speak(keyId, model, text, labels, { voice: voice.trim(), response_format: format }), false);
      }
    } catch (e) {
      if ((e as Error).name !== "AbortError") setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="panel-grid">
      <div className="card form">
        <TextArea caption="Text" rows={6} value={text} onChange={(e) => setText(e.target.value)} maxLength={MAX_SPEECH_CHARS} />
        <div className="row">
          {listed.length > 0 && (
            <Select
              caption="Voice"
              hint="The voices the model's deployments list, the default first."
              value={choice}
              onChange={setChoice}
              options={[
                ...listed.map((v) => ({ value: v.id, label: v.name ?? v.id, note: v.name ? v.id : undefined })),
                ...(open ? [{ value: ANY_VOICE, label: "Other voice…" }] : []),
              ]}
            />
          )}
          {choice === ANY_VOICE && (
            <TextInput
              caption={listed.length ? "Voice id" : "Voice"}
              hint={
                listed.length
                  ? "Passed to the provider as is."
                  : "OpenAI's voices by name (alloy, coral, marin, …); for Mistral a voice id such as voice_478. Empty for the default voice: alloy on OpenAI, gb_jane_neutral on Mistral. List the voices on the deployment to pick them here."
              }
              mono
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              placeholder="default"
            />
          )}
          <Select
            caption="Format"
            hint="pcm plays while it arrives, from the first words on; the other formats play once all of the audio has."
            value={format}
            onChange={setFormat}
            options={SPEECH_FORMATS}
          />
        </div>
        <div className="panel-actions">
          <CurlButton request={() => ({ path: "/v1/audio/speech", body, labels })} keyName={keyName} disabled={!text.trim()} />
          {player.playing ? (
            <Button variant="primary" onClick={player.stop}>
              Stop
            </Button>
          ) : (
            <Button variant="primary" onClick={submit} disabled={busy || !text.trim()}>
              {busy ? "Speaking…" : "Speak"}
            </Button>
          )}
        </div>
      </div>
      <div className="card">
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        {!result && !error && <div className="muted">The audio plays here.</div>}
        {result && url && (
          <>
            <MetaLine
              meta={result.meta}
              extra={`${result.streamed ? "to first sound · " : ""}${compact(result.characters)} characters · ${bytes(result.audio.size)}`}
            />
            <audio className="audio-preview" controls autoPlay={!result.streamed} src={url} />
            <a className="small" href={url} download={`speech.${result.audio.type === "audio/wav" ? "wav" : format}`}>
              Download
            </a>
          </>
        )}
      </div>
    </div>
  );
}

/** An object URL for `blob` while it is shown, revoked after. */
function useObjectUrl(blob: Blob | null): string | null {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    if (!blob) return setUrl(null);
    const u = URL.createObjectURL(blob);
    setUrl(u);
    return () => URL.revokeObjectURL(u);
  }, [blob]);
  return url;
}

// ---------------------------------------------------------------- realtime speech to text

/** A realtime transcription model: the microphone streams to it, and the words appear as they are said. */
export function LiveTranscriptionPanel({ keyId, model }: PanelProps) {
  const live = useLiveTranscription();
  const [language, setLanguage] = useSessionState("playground.live.language", "");
  const [text, setText] = useState("");
  const [final, setFinal] = useState<{ transcript: string; seconds: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [started, setStarted] = useState(0);

  const toggle = async () => {
    setError(null);
    if (!live.live) {
      setText("");
      setFinal(null);
      try {
        await live.start(keyId, model, setText, setError, language.trim() || undefined);
        setStarted(performance.now());
      } catch (e) {
        setError(microphoneError(e));
      }
      return;
    }
    setBusy(true);
    try {
      const transcript = await live.stop();
      setFinal({ transcript: transcript ?? text, seconds: (performance.now() - started) / 1000 });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="panel-grid">
      <div className="card form">
        <div className="audio-source">
          <Button variant={live.live ? undefined : "primary"} onClick={toggle} disabled={!live.supported || busy} aria-pressed={live.live}>
            {live.live ? <StopIcon /> : <MicIcon />} {live.live ? "Stop" : busy ? "Finishing…" : "Start speaking"}
          </Button>
          {live.live && <span className="muted small">Listening: the words appear as you speak.</span>}
        </div>
        <TextInput
          caption="Language"
          hint="The audio's language as an ISO-639-1 code, e.g. en or de. Optional; it improves accuracy. Fixed while listening."
          value={language}
          onChange={(e) => setLanguage(e.target.value)}
          placeholder="detect"
          disabled={live.live}
        />
        <p className="muted small">
          Streams to <span className="mono">/v1/realtime?intent=transcription</span> with OpenAI&apos;s Realtime protocol, as PCM16 at 24
          kHz.
        </p>
      </div>
      <div className="card">
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        {!text && !final && !error && <div className="muted">Start speaking, and the transcript appears here as you talk.</div>}
        {final && <MetaLine meta={{ model, latency: final.seconds * 1000 }} extra="final transcript" />}
        {(text || final) && <pre className="transcript">{final?.transcript ?? text}</pre>}
      </div>
    </div>
  );
}
