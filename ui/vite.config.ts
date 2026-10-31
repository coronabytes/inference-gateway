import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const gateway = process.env.GATEWAY_URL ?? "http://localhost:8080";
// The gateway sees the console's own address, which it puts in links it hands out (the agent cards,
// the redirect URI of upstream sign-ins).
const keepHost = { target: gateway, changeOrigin: false };

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/admin/api": keepHost,
      "/admin/auth/oauth": keepHost,
      "/v1": gateway,
      "/mcp": keepHost,
      "/a2a": keepHost,
      // The OAuth sign-in of MCP clients: its addresses are the console's, where people approve it.
      "/.well-known": keepHost,
      "/oauth": keepHost,
      "/admin/openapi.json": gateway,
      "/admin/docs": gateway,
    },
  },
  build: { outDir: "dist", sourcemap: false },
});
