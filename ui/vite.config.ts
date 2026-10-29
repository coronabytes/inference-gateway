import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const gateway = process.env.GATEWAY_URL ?? "http://localhost:8080";

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/admin/api": gateway,
      "/v1": gateway,
      "/admin/openapi.json": gateway,
      "/admin/docs": gateway,
    },
  },
  build: { outDir: "dist", sourcemap: false },
});
