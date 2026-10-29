// The API reference at /admin/docs/: Scalar, reading this gateway's OpenAPI document.

// The theme picked in the admin console's top bar, else the system's (see src/theme.ts).
function savedTheme() {
  try {
    const saved = localStorage.getItem("gateway-theme");
    if (saved === "light" || saved === "dark") return saved;
  } catch {
    // Storage can be blocked; follow the system then.
  }
  return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

Scalar.createApiReference("#app", {
  url: "/admin/openapi.json",
  forceDarkModeState: savedTheme(),
  // Nothing on this page talks to anyone but the gateway (the Content-Security-Policy enforces
  // it too): requests from "Try it" carry the visitor's API key. So no web fonts or telemetry,
  // and none of Scalar's hosted services: its developer toolbar (which also offers to upload the
  // document to generate SDKs), Ask AI and MCP.
  withDefaultFonts: false,
  telemetry: false,
  showDeveloperTools: "never",
  agent: { disabled: true },
  mcp: { disabled: true },
  persistAuth: false,
  authentication: { preferredSecurityScheme: "virtual_key" },
});

// The console shows one CSP error from this page, and it is expected: a library in the bundle
// (Zod) probes whether `Function("")` is allowed, and uses its slower path when, as here, the
// policy forbids evaluating strings.
