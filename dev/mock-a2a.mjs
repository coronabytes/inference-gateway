// Mock A2A agent for local end-to-end testing of the gateway's A2A proxy. Speaks A2A 1.0 over
// JSON-RPC (POST /a2a) and HTTP+JSON (/rest/…), streaming or not, with its card at
// /.well-known/agent-card.json. Requires the bearer token "mock-a2a-secret" when MOCK_A2A_TOKEN is
// not set to something else ("" turns the check off).
//
//   node dev/mock-a2a.mjs            # listens on :9997
//
// Messages are echoed back as a completed task's artifact. A message containing "fail" fails its
// task, one containing "ask" leaves it waiting for input (send another message with its taskId).
import http from "node:http";
import { randomUUID } from "node:crypto";

const PORT = Number(process.env.PORT ?? 9997);
const TOKEN = process.env.MOCK_A2A_TOKEN ?? "mock-a2a-secret";
const BASE = `http://127.0.0.1:${PORT}`;
const tasks = new Map();

const card = {
  name: "Mock agent",
  description: "Echoes messages back. 'fail' fails the task, 'ask' asks for more input.",
  version: "1.0.0",
  supportedInterfaces: [
    { url: `${BASE}/a2a`, protocolBinding: "JSONRPC", protocolVersion: "1.0" },
    { url: `${BASE}/rest`, protocolBinding: "HTTP+JSON", protocolVersion: "1.0" },
  ],
  capabilities: { streaming: true, extendedAgentCard: true },
  defaultInputModes: ["text/plain"],
  defaultOutputModes: ["text/plain"],
  skills: [{ id: "echo", name: "Echo", description: "Repeats what it is told.", tags: ["test"] }],
  securitySchemes: { bearer: { httpAuthSecurityScheme: { scheme: "Bearer" } } },
  securityRequirements: [{ schemes: { bearer: { list: [] } } }],
};

const textOf = (message) => (message?.parts ?? []).map((p) => p.text ?? "").join(" ");

/** The task a message creates or continues, and the events of working on it. */
function work(message) {
  const text = textOf(message);
  let task = message?.taskId && tasks.get(message.taskId);
  if (!task) {
    task = { id: randomUUID(), contextId: message?.contextId ?? randomUUID(), status: { state: "TASK_STATE_SUBMITTED" }, artifacts: [], history: [] };
    tasks.set(task.id, task);
  }
  task.history.push({ ...message, taskId: task.id, contextId: task.contextId });
  const state = text.includes("fail") ? "TASK_STATE_FAILED" : text.includes("ask") ? "TASK_STATE_INPUT_REQUIRED" : "TASK_STATE_COMPLETED";
  const artifact = { artifactId: randomUUID(), parts: [{ text: `echo: ${text}` }] };
  const ids = { taskId: task.id, contextId: task.contextId };
  const events = [
    { task: { ...task, history: undefined } },
    { statusUpdate: { ...ids, status: { state: "TASK_STATE_WORKING" } } },
    { artifactUpdate: { ...ids, artifact } },
    { statusUpdate: { ...ids, status: { state } } },
  ];
  task.status = { state };
  task.artifacts.push(artifact);
  return { task, events };
}

function sse(res, events, wrap) {
  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  let i = 0;
  const next = () => {
    if (i === events.length) return res.end();
    res.write(`data: ${JSON.stringify(wrap(events[i++]))}\n\n`);
    setTimeout(next, 50);
  };
  next();
}

const json = (res, status, body) => res.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));

function jsonrpc(req, res, msg) {
  const reply = (result) => json(res, 200, { jsonrpc: "2.0", id: msg.id, result });
  const error = (code, message) => json(res, 200, { jsonrpc: "2.0", id: msg.id, error: { code, message } });
  const p = msg.params ?? {};
  switch (msg.method) {
    case "SendMessage":
      return reply({ task: work(p.message).task });
    case "SendStreamingMessage":
      return sse(res, work(p.message).events, (result) => ({ jsonrpc: "2.0", id: msg.id, result }));
    case "GetTask":
      return tasks.has(p.id) ? reply(tasks.get(p.id)) : error(-32001, "Task not found");
    case "CancelTask": {
      const t = tasks.get(p.id);
      if (!t) return error(-32001, "Task not found");
      t.status = { state: "TASK_STATE_CANCELED" };
      return reply(t);
    }
    case "ListTasks":
      return reply({ tasks: [...tasks.values()] });
    case "GetExtendedAgentCard":
      return reply({ ...card, description: "the extended card, with the agent's own addresses" });
    default:
      return error(-32601, `Method not found: ${msg.method}`);
  }
}

function rest(req, res, path, body) {
  if (req.method === "POST" && path === "message:send") return json(res, 200, { task: work(body.message).task });
  if (req.method === "POST" && path === "message:stream") return sse(res, work(body.message).events, (e) => e);
  if (req.method === "GET" && path === "tasks") return json(res, 200, { tasks: [...tasks.values()] });
  const m = path.match(/^tasks\/([^/:]+)(:cancel)?$/);
  if (m) {
    const t = tasks.get(m[1]);
    if (!t) return json(res, 404, { error: { code: 404, message: "Task not found" } });
    if (m[2]) t.status = { state: "TASK_STATE_CANCELED" };
    return json(res, 200, t);
  }
  json(res, 404, { error: { code: 404, message: `no route ${req.method} ${path}` } });
}

http
  .createServer(async (req, res) => {
    const url = new URL(req.url, BASE);
    console.log(req.method, req.url);
    if (TOKEN && req.headers.authorization !== `Bearer ${TOKEN}`) return json(res, 401, { error: "unauthorized" });
    if (url.pathname === "/.well-known/agent-card.json") return json(res, 200, card);
    let raw = "";
    for await (const chunk of req) raw += chunk;
    let body = {};
    try {
      body = raw ? JSON.parse(raw) : {};
    } catch {
      return json(res, 400, { jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } });
    }
    if (url.pathname === "/a2a" && req.method === "POST") return jsonrpc(req, res, body);
    if (url.pathname.startsWith("/rest/")) return rest(req, res, url.pathname.slice("/rest/".length), body);
    res.writeHead(404).end();
  })
  .listen(PORT, "127.0.0.1", () => console.log(`mock A2A agent on ${BASE}, card at ${BASE}/.well-known/agent-card.json`));
