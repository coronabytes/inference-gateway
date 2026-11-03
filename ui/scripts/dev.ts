// The console's dev server, on http://localhost:5173: the app with hot reload, the files in public/,
// and the gateway's own paths passed on to GATEWAY_URL (default http://localhost:8080).

import { readdir } from "node:fs/promises";
import path from "node:path";
import index from "../index.html";
import { fetchScalar } from "./fetch-scalar";

const gateway = new URL(process.env.GATEWAY_URL ?? "http://localhost:8080");
const port = Number(process.env.PORT ?? 5173);

// The paths the gateway answers. Requests keep the console's own address as their Host, which the
// gateway puts in links it hands out (the agent cards, the redirect URI of upstream sign-ins, the
// OAuth sign-in of MCP clients, whose addresses are the console's, where people approve it).
const PROXIED = ["/admin/api", "/admin/auth/oauth", "/v1", "/mcp", "/a2a", "/.well-known", "/oauth", "/admin/openapi.json", "/admin/docs"];

/** A browser's WebSocket, and the gateway's that it is relayed to. */
interface Relay {
  target: string;
  headers: Record<string, string>;
  protocols: string[];
  upstream?: WebSocket;
  /** What the browser sent while the gateway's socket was still opening. */
  queue: (string | Uint8Array<ArrayBuffer>)[];
}

async function proxy(req: Request, server: Bun.Server<Relay>) {
  const url = new URL(req.url);
  const target = new URL(url.pathname + url.search, gateway);
  // WebSockets (the playground's realtime sessions) are relayed: only from the console's own pages,
  // as the gateway checks when it is not behind this server.
  if (req.headers.get("upgrade")?.toLowerCase() === "websocket") {
    const origin = req.headers.get("origin");
    if (origin && new URL(origin).host !== url.host) return new Response("cross-origin WebSocket", { status: 403 });
    const headers: Record<string, string> = {};
    for (const name of ["cookie", "authorization", "x-gateway-labels"]) {
      const v = req.headers.get(name);
      if (v) headers[name] = v;
    }
    const protocols = (req.headers.get("sec-websocket-protocol") ?? "")
      .split(",")
      .map((p) => p.trim())
      .filter(Boolean);
    target.protocol = gateway.protocol === "https:" ? "wss:" : "ws:";
    const relay: Relay = { target: target.href, headers, protocols, queue: [] };
    // The gateway picks `realtime` among the protocols offered; the browser needs the same answer.
    const answer = protocols.includes("realtime") ? { "sec-websocket-protocol": "realtime" } : undefined;
    if (server.upgrade(req, { data: relay, headers: answer })) return undefined;
    return new Response("WebSocket upgrade failed", { status: 400 });
  }
  try {
    const res = await fetch(target, {
      method: req.method,
      headers: req.headers,
      body: req.body,
      redirect: "manual",
      decompress: false,
    });
    return new Response(res.body, res);
  } catch (e) {
    return new Response(`gateway at ${gateway.origin} unreachable: ${e}`, { status: 502 });
  }
}

const publicDir = path.join(import.meta.dir, "..", "public");
const publicFiles = Object.fromEntries(
  (await readdir(publicDir, { recursive: true, withFileTypes: true }))
    .filter((f) => f.isFile())
    .map((f) => {
      const file = path.join(f.parentPath, f.name);
      return [`/${path.relative(publicDir, file).replaceAll("\\", "/")}`, Bun.file(file)];
    }),
);

await fetchScalar();

const server = Bun.serve<Relay>({
  port,
  development: { hmr: true, console: true },
  websocket: {
    open(ws) {
      const r = ws.data;
      const up = new WebSocket(r.target, { headers: r.headers, protocols: r.protocols } as unknown as string[]);
      r.upstream = up;
      up.onopen = () => {
        for (const m of r.queue.splice(0)) up.send(m);
      };
      up.onmessage = (e) => ws.send(typeof e.data === "string" ? e.data : new Uint8Array(e.data as ArrayBuffer));
      up.onclose = (e) => ws.close(e.code === 1005 ? 1000 : e.code, e.reason);
      up.onerror = () => ws.close(1011, "gateway unreachable");
    },
    message(ws, message) {
      const up = ws.data.upstream;
      const m = typeof message === "string" ? message : new Uint8Array(message);
      if (up?.readyState === WebSocket.OPEN) up.send(m);
      else ws.data.queue.push(m);
    },
    close(ws) {
      ws.data.upstream?.close();
    },
  },
  routes: {
    ...publicFiles,
    ...Object.fromEntries(
      PROXIED.flatMap((p) => [
        [p, proxy],
        [`${p}/*`, proxy],
      ]),
    ),
    "/*": index,
  },
});

console.log(`console on ${server.url}, gateway at ${gateway.origin}`);
