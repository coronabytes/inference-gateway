// Mock MCP server for local end-to-end testing of the gateway's MCP proxy. Speaks Streamable HTTP
// at /mcp, with sessions, and answers tool calls as JSON or, with `?sse` in the server's URL, as
// server-sent events (a progress notification, then the reply). Requires the bearer token
// "mock-mcp-secret" when MOCK_MCP_TOKEN is not set to something else ("" turns the check off).
//
//   node dev/mock-mcp.mjs            # listens on :9998
//
// Tools: `echo` returns its text, `add` adds two numbers, `fail` reports a tool error, `slow`
// answers after `ms` milliseconds (to exercise timeouts and clients that go away).
import http from "node:http";
import { randomUUID } from "node:crypto";

const PORT = Number(process.env.PORT ?? 9998);
const TOKEN = process.env.MOCK_MCP_TOKEN ?? "mock-mcp-secret";
const sessions = new Set();

const TOOLS = [
  { name: "echo", description: "Returns the text it is given.", inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] } },
  { name: "add", description: "Adds two numbers.", inputSchema: { type: "object", properties: { a: { type: "number" }, b: { type: "number" } }, required: ["a", "b"] } },
  { name: "fail", description: "Always reports a tool error.", inputSchema: { type: "object", properties: {} } },
  { name: "slow", description: "Answers after a delay.", inputSchema: { type: "object", properties: { ms: { type: "number" } } } },
];

const text = (t, isError = false) => ({ content: [{ type: "text", text: String(t) }], ...(isError && { isError: true }) });

async function call(name, args = {}) {
  switch (name) {
    case "echo":
      return text(args.text ?? "");
    case "add":
      return text(Number(args.a) + Number(args.b));
    case "fail":
      return text("this tool always fails", true);
    case "slow":
      await new Promise((r) => setTimeout(r, Number(args.ms ?? 2000)));
      return text(`waited ${args.ms ?? 2000} ms`);
    default:
      return null;
  }
}

async function answer(msg) {
  const reply = (result) => ({ jsonrpc: "2.0", id: msg.id, result });
  const error = (code, message) => ({ jsonrpc: "2.0", id: msg.id, error: { code, message } });
  switch (msg.method) {
    case "initialize":
      return reply({
        protocolVersion: msg.params?.protocolVersion ?? "2025-11-25",
        capabilities: { tools: {} },
        serverInfo: { name: "mock-mcp", version: "1.0.0" },
      });
    case "ping":
      return reply({});
    case "tools/list":
      return reply({ tools: TOOLS });
    case "tools/call": {
      const result = await call(msg.params?.name, msg.params?.arguments);
      return result ? reply(result) : error(-32602, `unknown tool: ${msg.params?.name}`);
    }
    default:
      return error(-32601, `method not found: ${msg.method}`);
  }
}

http
  .createServer(async (req, res) => {
    const url = new URL(req.url, "http://localhost");
    console.log(req.method, req.url, req.headers["mcp-session-id"] ?? "");
    if (url.pathname !== "/mcp") return res.writeHead(404).end();
    if (TOKEN && req.headers.authorization !== `Bearer ${TOKEN}`) {
      return res.writeHead(401, { "www-authenticate": 'Bearer resource_metadata="http://localhost/.well-known/oauth-protected-resource"' }).end();
    }
    const session = req.headers["mcp-session-id"];
    if (req.method === "DELETE") {
      sessions.delete(session);
      return res.writeHead(200).end();
    }
    if (req.method === "GET") return res.writeHead(405, { allow: "POST, DELETE" }).end();
    if (req.method !== "POST") return res.writeHead(405).end();

    let body = "";
    for await (const chunk of req) body += chunk;
    let msg;
    try {
      msg = JSON.parse(body);
    } catch {
      return res.writeHead(400, { "content-type": "application/json" }).end(JSON.stringify({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } }));
    }
    const headers = { "content-type": "application/json" };
    if (msg.method === "initialize") {
      const id = randomUUID();
      sessions.add(id);
      headers["mcp-session-id"] = id;
    } else if (!sessions.has(session)) {
      return res.writeHead(404).end();
    }
    // Notifications and responses get no answer.
    if (msg.id === undefined) return res.writeHead(202).end();

    if (url.searchParams.has("sse") && msg.method === "tools/call") {
      res.writeHead(200, { ...headers, "content-type": "text/event-stream" });
      const token = msg.params?._meta?.progressToken;
      if (token !== undefined) {
        res.write(`data: ${JSON.stringify({ jsonrpc: "2.0", method: "notifications/progress", params: { progressToken: token, progress: 0 } })}\n\n`);
      }
      const reply = await answer(msg);
      res.write(`event: message\ndata: ${JSON.stringify(reply)}\n\n`);
      return res.end();
    }
    res.writeHead(200, headers).end(JSON.stringify(await answer(msg)));
  })
  .listen(PORT, "127.0.0.1", () => console.log(`mock MCP server on http://127.0.0.1:${PORT}/mcp`));
