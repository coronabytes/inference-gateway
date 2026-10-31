// Mock MCP client for local end-to-end testing of the gateway's OAuth sign-in for MCP clients. It
// identifies with a Client ID Metadata Document it serves itself, and signs in the way MCP clients
// do: an unauthenticated request, the WWW-Authenticate challenge, the protected resource and
// authorization server metadata, then the authorization code flow with PKCE and `resource`.
//
//   node dev/mock-mcp-client.mjs     # listens on :9996, against the console's address
//   GATEWAY_URL=http://localhost:5173 MCP_PATH=/mcp/mock node dev/mock-mcp-client.mjs
//
// Open http://127.0.0.1:9996 and click Connect. After the approval in the console it lists the
// tools with its access token, refreshes the token, and lists them again.
import http from "node:http";
import { createHash, randomBytes } from "node:crypto";

const PORT = Number(process.env.PORT ?? 9996);
const BASE = `http://127.0.0.1:${PORT}`;
const GATEWAY = process.env.GATEWAY_URL ?? "http://localhost:5173";
const MCP = `${GATEWAY}${process.env.MCP_PATH ?? "/mcp"}`;
const CLIENT_ID = `${BASE}/client.json`;
const REDIRECT = `${BASE}/callback`;
const b64url = (b) => b.toString("base64url");
/** Sign-ins in progress, by state. */
const pending = new Map();

const document = {
  client_id: CLIENT_ID,
  client_name: "Mock MCP client",
  client_uri: BASE,
  redirect_uris: [REDIRECT],
  grant_types: ["authorization_code", "refresh_token"],
  response_types: ["code"],
  token_endpoint_auth_method: "none",
};

const page = (res, body) => res.writeHead(200, { "content-type": "text/html; charset=utf-8" }).end(`<!doctype html><body style="font-family:system-ui;max-width:50em;margin:3em auto">${body}`);
const pre = (v) => `<pre>${JSON.stringify(v, null, 2).replace(/</g, "&lt;")}</pre>`;

async function listTools(token) {
  const r = await fetch(MCP, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json", accept: "application/json, text/event-stream", "mcp-protocol-version": "2025-06-18" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
  });
  const text = await r.text();
  try {
    return { status: r.status, body: JSON.parse(text) };
  } catch {
    return { status: r.status, body: text.slice(0, 2000) };
  }
}

/** Discovery as MCP clients do it: the challenge, then both metadata documents. */
async function discover() {
  const r = await fetch(MCP, { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
  const challenge = r.headers.get("www-authenticate") ?? "";
  const prmUrl = challenge.match(/resource_metadata="([^"]+)"/)?.[1];
  if (r.status !== 401 || !prmUrl) throw new Error(`expected a 401 with resource_metadata, got ${r.status}: ${challenge}`);
  const prm = await (await fetch(prmUrl)).json();
  const issuer = prm.authorization_servers[0];
  const as = await (await fetch(`${issuer}/.well-known/oauth-authorization-server`)).json();
  if (as.issuer !== issuer) throw new Error(`issuer mismatch: ${as.issuer} vs ${issuer}`);
  if (!as.client_id_metadata_document_supported) throw new Error("the server does not take client metadata documents");
  if (!as.code_challenge_methods_supported?.includes("S256")) throw new Error("the server does not support PKCE S256");
  return { challenge, prm, as };
}

http
  .createServer(async (req, res) => {
    const url = new URL(req.url, BASE);
    console.log(req.method, req.url);
    try {
      if (url.pathname === "/client.json") return res.writeHead(200, { "content-type": "application/json", "cache-control": "max-age=60" }).end(JSON.stringify(document));
      if (url.pathname === "/") return page(res, `<h1>Mock MCP client</h1><p>MCP server: ${MCP}</p><p><a href="/connect">Connect</a></p>`);
      if (url.pathname === "/connect") {
        const { prm, as } = await discover();
        const verifier = b64url(randomBytes(32));
        const state = b64url(randomBytes(16));
        pending.set(state, { verifier, as, resource: prm.resource });
        const q = new URLSearchParams({
          response_type: "code",
          client_id: CLIENT_ID,
          redirect_uri: REDIRECT,
          code_challenge: b64url(createHash("sha256").update(verifier).digest()),
          code_challenge_method: "S256",
          resource: prm.resource,
          scope: (prm.scopes_supported ?? []).join(" "),
          state,
        });
        return res.writeHead(302, { location: `${as.authorization_endpoint}?${q}` }).end();
      }
      if (url.pathname === "/callback") {
        const p = pending.get(url.searchParams.get("state"));
        if (!p) return page(res, "<h1>Unknown state</h1>");
        pending.delete(url.searchParams.get("state"));
        if (url.searchParams.get("iss") !== p.as.issuer) return page(res, `<h1>Wrong iss</h1>${pre(Object.fromEntries(url.searchParams))}`);
        if (url.searchParams.get("error")) return page(res, `<h1>Not connected</h1>${pre(Object.fromEntries(url.searchParams))}`);
        const form = (fields) => ({ method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body: new URLSearchParams(fields) });
        const tokens = await (
          await fetch(p.as.token_endpoint, form({ grant_type: "authorization_code", code: url.searchParams.get("code"), client_id: CLIENT_ID, redirect_uri: REDIRECT, code_verifier: p.verifier, resource: p.resource }))
        ).json();
        if (!tokens.access_token) return page(res, `<h1>Token exchange failed</h1>${pre(tokens)}`);
        const first = await listTools(tokens.access_token);
        const refreshed = await (await fetch(p.as.token_endpoint, form({ grant_type: "refresh_token", refresh_token: tokens.refresh_token, client_id: CLIENT_ID }))).json();
        const reused = await (await fetch(p.as.token_endpoint, form({ grant_type: "refresh_token", refresh_token: tokens.refresh_token, client_id: CLIENT_ID }))).json();
        const second = refreshed.access_token ? await listTools(refreshed.access_token) : null;
        const old = await listTools(tokens.access_token);
        const tools = (r) => r?.body?.result?.tools?.map((t) => t.name) ?? r;
        return page(
          res,
          `<h1>Connected</h1>${pre({
            token: { expires_in: tokens.expires_in, scope: tokens.scope },
            tools: tools(first),
            after_refresh: tools(second),
            refresh_token_reused: reused,
            old_access_token_after_refresh: old.status,
          })}`,
        );
      }
      res.writeHead(404).end();
    } catch (e) {
      page(res, `<h1>Failed</h1><pre>${String(e?.stack ?? e)}</pre>`);
    }
  })
  .listen(PORT, "127.0.0.1", () => console.log(`mock MCP client on ${BASE}, against ${MCP}`));
