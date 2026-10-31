import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { adminFetch, api } from "../api";
import { Banner, SplitButton } from "../components/controls";
import { CurlDialog, type CurlRequest } from "../components/CurlButton";
import { ErrorBanner } from "../components/ui";
import { ms } from "../format";
import { labelHeaders, type Labels } from "../labels";
import { sse } from "../sse";

/** An A2A message's or artifact's parts as text: text as it is, data as JSON, files by name. */
function partsText(parts: any[] | undefined): string {
  return (parts ?? [])
    .map((p) => {
      if (typeof p.text === "string") return p.text;
      if (p.data !== undefined) return JSON.stringify(p.data, null, 2);
      const f = p.file ?? p;
      if (f.uri || f.url || f.bytes || f.raw) return `[file${f.name || f.filename ? ` ${f.name ?? f.filename}` : ""}]`;
      return JSON.stringify(p);
    })
    .join("\n");
}

/** A task state as A2A 0.3 writes it (`input-required`); 1.0 writes `TASK_STATE_INPUT_REQUIRED`. */
const stateName = (s?: string) => s?.replace(/^TASK_STATE_/, "").toLowerCase().replace(/_/g, "-");

/** States in which the task waits for the client: the next message continues it. */
const WAITING = ["input-required", "auth-required"];

/** An answer or stream event in either protocol version: 1.0 wraps it (`{task}`, `{statusUpdate}`, …),
 *  0.3 names its `kind`. */
function unwrap(e: any): { task?: any; message?: any; status?: any; artifact?: any } {
  switch (e?.kind) {
    case "task":
      return { task: e };
    case "message":
      return { message: e };
    case "status-update":
      return { status: e };
    case "artifact-update":
      return { artifact: e };
  }
  return { task: e?.task, message: e?.message, status: e?.statusUpdate, artifact: e?.artifactUpdate };
}

/** What the agent answered to one message: the task it created or continued, or a message. */
interface Reply {
  state?: string;
  taskId?: string;
  contextId?: string;
  text: string;
  artifacts: { id: string; name?: string; text: string }[];
  /** The message of the task's latest status, such as the question it waits on. */
  statusText?: string;
}

/** An artifact, new or again: a task lists the ones streamed before, `append` adds to one. */
function putArtifact(r: Reply, a: any, append: boolean) {
  const id = a?.artifactId ?? String(r.artifacts.length);
  const text = partsText(a?.parts);
  const cur = r.artifacts.find((x) => x.id === id);
  if (cur) cur.text = append ? cur.text + text : text;
  else r.artifacts.push({ id, name: a?.name, text });
}

function apply(r: Reply, e: any) {
  const { task, message, status, artifact } = unwrap(e);
  if (task) {
    r.taskId = task.id;
    r.contextId = task.contextId;
    r.state = stateName(task.status?.state) ?? r.state;
    for (const a of task.artifacts ?? []) putArtifact(r, a, false);
    if (task.status?.message) r.statusText = partsText(task.status.message.parts);
  }
  if (message) {
    r.text += (r.text ? "\n" : "") + partsText(message.parts);
    r.contextId ??= message.contextId;
    r.taskId ??= message.taskId;
  }
  if (status) {
    r.state = stateName(status.status?.state) ?? r.state;
    r.taskId ??= status.taskId;
    r.contextId ??= status.contextId;
    if (status.status?.message) r.statusText = partsText(status.status.message.parts);
  }
  if (artifact) putArtifact(r, artifact.artifact, !!artifact.append);
}

interface Turn {
  role: "user" | "agent";
  text: string;
  reply?: Reply;
  pending?: boolean;
  error?: string;
  latency?: number;
}

/** The JSON-RPC interface the card offers through the gateway, and its protocol version. */
function rpcOf(card: any): { version: string } | null {
  if (Array.isArray(card?.supportedInterfaces)) {
    const i = card.supportedInterfaces.find((x: any) => x.protocolBinding === "JSONRPC");
    return i ? { version: i.protocolVersion ?? "1.0" } : null;
  }
  return (card?.preferredTransport ?? "JSONRPC") === "JSONRPC" ? { version: card?.protocolVersion ?? "0.3.0" } : null;
}

/**
 * The playground's A2A mode: messages to an agent as the key, through the gateway's `/a2a/{agent}`
 * JSON-RPC interface, in the protocol version the agent's card names, streamed where it streams.
 * The conversation keeps its context, and a task waiting for input is continued by the next message.
 */
export function AgentPanel({ keyId, keyName, agent, labels }: { keyId: string; keyName?: string; agent: string; labels: Labels }) {
  const [card, setCard] = useState<any | null>(null);
  const [turns, setTurns] = useState<Turn[]>([]);
  const [contextId, setContextId] = useState<string | null>(null);
  /** A task waiting for input, which the next message continues. */
  const [taskId, setTaskId] = useState<string | null>(null);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [curlShown, setCurlShown] = useState<CurlRequest | null>(null);
  const abort = useRef<AbortController | null>(null);
  const scroller = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let current = true;
    api<any>(`/playground/a2a/${encodeURIComponent(agent)}/card?key_id=${encodeURIComponent(keyId)}`)
      .then((c) => current && setCard(c))
      .catch((e) => current && setError((e as Error).message));
    return () => {
      current = false;
    };
  }, [agent, keyId]);

  useEffect(() => {
    scroller.current?.scrollTo({ top: scroller.current.scrollHeight });
  }, [turns]);

  const rpc = card ? rpcOf(card) : null;
  const v1 = !!rpc && !rpc.version.startsWith("0.");
  const streaming = !!card?.capabilities?.streaming;

  /** The JSON-RPC request Send would make for `text`. */
  const request = (text: string) => {
    const ids = { ...(contextId && { contextId }), ...(taskId && { taskId }) };
    const message = v1
      ? { messageId: crypto.randomUUID(), role: "ROLE_USER", parts: [{ text }], ...ids }
      : { kind: "message", messageId: crypto.randomUUID(), role: "user", parts: [{ kind: "text", text }], ...ids };
    const method = streaming ? (v1 ? "SendStreamingMessage" : "message/stream") : v1 ? "SendMessage" : "message/send";
    return { body: { jsonrpc: "2.0", id: 1, method, params: { message } }, headers: { "A2A-Version": rpc?.version ?? "1.0" } };
  };

  const update = (patch: Partial<Turn>) => setTurns((ts) => ts.map((t, i) => (i === ts.length - 1 ? { ...t, ...patch } : t)));

  const send = async () => {
    const text = input.trim();
    if (!text || !rpc || busy) return;
    const { body, headers } = request(text);
    setTurns((ts) => [...ts, { role: "user", text }, { role: "agent", text: "", pending: true }]);
    setInput("");
    setBusy(true);
    const controller = new AbortController();
    abort.current = controller;
    const started = performance.now();
    const reply: Reply = { text: "", artifacts: [] };
    try {
      const res = await adminFetch(`/playground/a2a/${encodeURIComponent(agent)}?key_id=${encodeURIComponent(keyId)}`, body, controller.signal, {
        ...labelHeaders(labels),
        ...headers,
      });
      if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        throw new Error(data?.error?.message ?? data?.error ?? `request failed (${res.status})`);
      }
      const take = (v: any) => {
        if (v?.error) throw new Error(v.error.message ?? JSON.stringify(v.error));
        apply(reply, v?.result);
      };
      if (res.headers.get("content-type")?.includes("event-stream")) {
        for await (const ev of sse(res)) {
          take(JSON.parse(ev.data));
          update({ reply: { ...reply, artifacts: reply.artifacts.map((a) => ({ ...a })) } });
        }
      } else {
        take(await res.json());
      }
      if (reply.contextId) setContextId(reply.contextId);
      setTaskId(reply.taskId && reply.state && WAITING.includes(reply.state) ? reply.taskId : null);
      update({ reply, pending: false, latency: performance.now() - started });
    } catch (e) {
      const aborted = (e as Error).name === "AbortError";
      update({ reply, pending: false, error: aborted ? "Stopped." : (e as Error).message });
    } finally {
      setBusy(false);
      abort.current = null;
    }
  };

  const onKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  };

  const restart = () => {
    abort.current?.abort();
    setTurns([]);
    setContextId(null);
    setTaskId(null);
  };

  const skills: any[] = card?.skills ?? [];
  return (
    <>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {card && !rpc && (
        <Banner tone="warning">
          The playground sends JSON-RPC, and this agent offers only HTTP+JSON. Clients reach it at <code>/a2a/{agent}/rest/…</code>.
        </Banner>
      )}
      {card && (
        <details className="system-prompt">
          <summary>Agent card</summary>
          <pre className="agent-card">{JSON.stringify(card, null, 2)}</pre>
        </details>
      )}
      <div className="chat">
        <div className="chat-log" ref={scroller}>
          {turns.length === 0 && (
            <div className="chat-empty muted">
              {card ? (
                <>
                  <strong>{card.name ?? agent}</strong>
                  {rpc && ` · A2A ${rpc.version}${streaming ? " · streams" : ""}`}
                  {card.description && <div>{card.description}</div>}
                  {skills.length > 0 && <div>Skills: {skills.map((s) => s.name ?? s.id).join(", ")}</div>}
                  <div className="agent-note">
                    Messages go to <code>/a2a/{agent}</code> as key <strong>{keyName ?? "…"}</strong>: logged, counted against its limits and
                    budget, and charged the agent's price.
                  </div>
                </>
              ) : (
                "Loading the agent's card…"
              )}
            </div>
          )}
          {turns.map((t, i) => (
            <div key={i} className={`bubble-row ${t.role === "user" ? "user" : "assistant"}`}>
              <div className={`bubble ${t.role === "user" ? "user" : "assistant"}${t.error ? " failed" : ""}`}>
                {t.role === "user" ? (
                  t.text
                ) : (
                  <>
                    {t.reply?.state && <span className={`a2a-state ${t.reply.state}`}>{t.reply.state}</span>}
                    {t.reply?.text}
                    {t.reply?.artifacts.map((a) => (
                      <div key={a.id} className="a2a-artifact">
                        <div className="mcp-label">{a.name || "Artifact"}</div>
                        {a.text}
                      </div>
                    ))}
                    {t.reply?.statusText && <div className="a2a-status">{t.reply.statusText}</div>}
                    {t.pending && !t.reply?.state && <span className="typing" aria-label="Waiting for the agent">…</span>}
                  </>
                )}
                {t.error && <div className="bubble-error">{t.error}</div>}
              </div>
              {t.role === "agent" && !t.pending && t.reply?.taskId && (
                <div className="bubble-meta">
                  {t.latency != null && `${ms(t.latency)} · `}task {t.reply.taskId.slice(0, 8)}
                  {t.reply.contextId && ` · context ${t.reply.contextId.slice(0, 8)}`}
                  {i === turns.length - 1 && taskId && " · waiting: the next message continues the task"}
                </div>
              )}
            </div>
          ))}
        </div>
        <div className="composer">
          <textarea
            rows={1}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={onKey}
            placeholder={rpc ? "Message… (Enter to send, Shift+Enter for a new line)" : "The agent offers no JSON-RPC interface"}
            disabled={!rpc}
            aria-label="Message"
          />
          <SplitButton
            variant={busy ? undefined : "primary"}
            onClick={busy ? () => abort.current?.abort() : () => send()}
            disabled={!busy && (!input.trim() || !rpc)}
            menuLabel="More send actions"
            items={[
              {
                label: "Show curl",
                title: "What Send would send, as a curl command to the gateway's /a2a endpoint.",
                disabled: !input.trim() || !rpc,
                onClick: () => {
                  const { body, headers } = request(input.trim());
                  setCurlShown({ path: `/a2a/${agent}`, body, labels, headers, stream: streaming });
                },
              },
              { label: "New conversation", disabled: !turns.length, onClick: restart },
            ]}
          >
            {busy ? "Stop" : "Send"}
          </SplitButton>
        </div>
      </div>
      {curlShown && <CurlDialog request={curlShown} keyName={keyName} onClose={() => setCurlShown(null)} />}
    </>
  );
}
