// Mock MCP server for local end-to-end testing of the gateway's MCP proxy. Speaks Streamable HTTP
// at /mcp, with sessions, and answers tool calls as JSON or, with `?sse` in the server's URL, as
// server-sent events (a progress notification, then the reply). Requires the bearer token
// "mock-mcp-secret" when MOCK_MCP_TOKEN is not set to something else ("" turns the check off).
//
//   node dev/mock-mcp.mjs            # listens on :9998
//
// Tools: `echo` returns its text, `add` adds two numbers, `fail` reports a tool error, `slow`
// answers after `ms` milliseconds (to exercise timeouts and clients that go away), `whoami` says
// which token called it.
//
// It is also its own OAuth authorization server, for the gateway's `oauth` and `oauth_user`
// authentication: protected resource metadata (RFC 9728) at /.well-known/oauth-protected-resource,
// server metadata (RFC 8414), dynamic client registration (RFC 7591), an authorization endpoint
// that approves at once, and a token endpoint for authorization codes (with PKCE), refresh tokens
// (rotated) and client credentials (client "gateway-client", secret "gateway-secret"). Its access
// tokens last MOCK_MCP_TOKEN_SECONDS (default 120) and are taken by /mcp besides the static token.
import http from "node:http";
import { createHash, randomUUID } from "node:crypto";

const PORT = Number(process.env.PORT ?? 9998);
const BASE = `http://localhost:${PORT}`;
const TOKEN = process.env.MOCK_MCP_TOKEN ?? "mock-mcp-secret";
const TOKEN_SECONDS = Number(process.env.MOCK_MCP_TOKEN_SECONDS ?? 120);
const sessions = new Set();

// ---- the authorization server
const clients = new Map([["gateway-client", { secret: "gateway-secret" }]]);
const codes = new Map(); // code -> { client, redirect, challenge, user }
const accessTokens = new Map(); // token -> { who, expires }
const refreshTokens = new Map(); // token -> who
let users = 0;

function issue(who) {
  const access = `mock-at-${randomUUID()}`;
  const refresh = `mock-rt-${randomUUID()}`;
  accessTokens.set(access, { who, expires: Date.now() + TOKEN_SECONDS * 1000 });
  refreshTokens.set(refresh, who);
  return { access_token: access, token_type: "Bearer", expires_in: TOKEN_SECONDS, refresh_token: refresh, scope: "tools" };
}

/** The token's holder, if it is one of ours and still valid. */
function holder(req) {
  const auth = req.headers.authorization ?? "";
  if (TOKEN && auth === `Bearer ${TOKEN}`) return "static token";
  const t = accessTokens.get(auth.replace(/^Bearer /, ""));
  return t && t.expires > Date.now() ? t.who : null;
}

const form = (raw) => Object.fromEntries(new URLSearchParams(raw));

function basicClient(req) {
  const m = (req.headers.authorization ?? "").match(/^Basic (.+)$/);
  if (!m) return null;
  const [id, secret] = Buffer.from(m[1], "base64").toString().split(":").map(decodeURIComponent);
  return { id, secret };
}

async function oauth(req, res, url) {
  const json = (status, body) => res.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
  const p = url.pathname;
  if (p.startsWith("/.well-known/oauth-protected-resource")) {
    return json(200, { resource: `${BASE}/mcp`, authorization_servers: [BASE], scopes_supported: ["tools"] });
  }
  if (p === "/.well-known/oauth-authorization-server") {
    return json(200, {
      issuer: BASE,
      authorization_endpoint: `${BASE}/oauth/authorize`,
      token_endpoint: `${BASE}/oauth/token`,
      registration_endpoint: `${BASE}/oauth/register`,
      token_endpoint_auth_methods_supported: ["none", "client_secret_basic", "client_secret_post"],
      code_challenge_methods_supported: ["S256"],
      scopes_supported: ["tools"],
    });
  }
  let raw = "";
  for await (const chunk of req) raw += chunk;
  if (p === "/oauth/register" && req.method === "POST") {
    const body = JSON.parse(raw || "{}");
    const id = `dyn-${randomUUID()}`;
    clients.set(id, { redirects: body.redirect_uris ?? [] });
    return json(201, { client_id: id, redirect_uris: body.redirect_uris, token_endpoint_auth_method: "none" });
  }
  if (p === "/oauth/authorize") {
    const q = Object.fromEntries(url.searchParams);
    const c = clients.get(q.client_id);
    if (!c || (c.redirects && !c.redirects.includes(q.redirect_uri))) return json(400, { error: "invalid_client" });
    if (q.code_challenge_method !== "S256" || !q.code_challenge) return json(400, { error: "invalid_request", error_description: "PKCE S256 required" });
    const code = randomUUID();
    codes.set(code, { client: q.client_id, redirect: q.redirect_uri, challenge: q.code_challenge, resource: q.resource, user: `user-${++users}` });
    const back = new URL(q.redirect_uri);
    back.searchParams.set("code", code);
    back.searchParams.set("state", q.state);
    return res.writeHead(302, { location: back.toString() }).end();
  }
  if (p === "/oauth/token" && req.method === "POST") {
    const f = form(raw);
    const basic = basicClient(req);
    const clientId = basic?.id ?? f.client_id;
    if (f.grant_type === "client_credentials") {
      const secret = basic?.secret ?? f.client_secret;
      if (clients.get(clientId)?.secret !== secret) return json(401, { error: "invalid_client" });
      const t = issue(`client ${clientId}`);
      delete t.refresh_token;
      return json(200, t);
    }
    if (f.grant_type === "authorization_code") {
      const c = codes.get(f.code);
      codes.delete(f.code);
      if (!c || c.client !== clientId || c.redirect !== f.redirect_uri) return json(400, { error: "invalid_grant" });
      const challenge = createHash("sha256").update(f.code_verifier ?? "").digest("base64url");
      if (challenge !== c.challenge) return json(400, { error: "invalid_grant", error_description: "PKCE verification failed" });
      return json(200, issue(c.user));
    }
    if (f.grant_type === "refresh_token") {
      const who = refreshTokens.get(f.refresh_token);
      if (!who) return json(400, { error: "invalid_grant" });
      refreshTokens.delete(f.refresh_token); // rotated
      return json(200, issue(who));
    }
    return json(400, { error: "unsupported_grant_type" });
  }
  return json(404, { error: "not_found" });
}

const TOOLS = [
  { name: "echo", description: "Returns the text it is given.", inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] } },
  { name: "add", description: "Adds two numbers.", inputSchema: { type: "object", properties: { a: { type: "number" }, b: { type: "number" } }, required: ["a", "b"] } },
  { name: "fail", description: "Always reports a tool error.", inputSchema: { type: "object", properties: {} } },
  { name: "whoami", description: "Says which token called it.", inputSchema: { type: "object", properties: {} } },
  { name: "slow", description: "Answers after a delay.", inputSchema: { type: "object", properties: { ms: { type: "number" } } } },
];

const text = (t, isError = false) => ({ content: [{ type: "text", text: String(t) }], ...(isError && { isError: true }) });

async function call(name, args = {}, who = "") {
  switch (name) {
    case "whoami":
      return text(who);
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

async function answer(msg, who) {
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
      const result = await call(msg.params?.name, msg.params?.arguments, who);
      return result ? reply(result) : error(-32602, `unknown tool: ${msg.params?.name}`);
    }
    default:
      return error(-32601, `method not found: ${msg.method}`);
  }
}

http
  .createServer(async (req, res) => {
    const url = new URL(req.url, BASE);
    console.log(req.method, req.url, req.headers["mcp-session-id"] ?? "");
    if (url.pathname.startsWith("/.well-known/") || url.pathname.startsWith("/oauth/")) return oauth(req, res, url);
    if (url.pathname !== "/mcp") return res.writeHead(404).end();
    const who = holder(req);
    if (TOKEN && !who) {
      return res.writeHead(401, { "www-authenticate": `Bearer resource_metadata="${BASE}/.well-known/oauth-protected-resource/mcp"` }).end();
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
      const reply = await answer(msg, who);
      res.write(`event: message\ndata: ${JSON.stringify(reply)}\n\n`);
      return res.end();
    }
    res.writeHead(200, headers).end(JSON.stringify(await answer(msg, who)));
  })
  .listen(PORT, "127.0.0.1", () => console.log(`mock MCP server on http://127.0.0.1:${PORT}/mcp`));
