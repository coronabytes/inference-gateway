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

async function proxy(req: Request) {
  const url = new URL(req.url);
  const target = new URL(url.pathname + url.search, gateway);
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

const server = Bun.serve({
  port,
  development: { hmr: true, console: true },
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
