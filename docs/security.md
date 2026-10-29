# Security

How the gateway protects credentials, data and itself. To report a vulnerability, see [SECURITY.md](../SECURITY.md).

- **Secrets.** Provider secrets are only entered through the console or admin API, encrypted at rest (AES-256-GCM) and never returned. Virtual keys are stored as hashes.
- **No prompt storage.** Prompts and responses are never logged, in the request log or in telemetry. The only content kept is the encrypted, expiring history for `previous_response_id`, and none at all with `store: false`. A guardrail sends the prompt's text to its provider; the request log keeps only the kinds of things it found.
- **Outbound connections.** The gateway connects only to public addresses unless you allow more (`GATEWAY_UPSTREAM_ALLOWED_NETWORKS`), never follows redirects, and always blocks cloud metadata addresses. A Google service account file cannot send its credentials anywhere but Google.
- **Console.** Admin sessions use secure, HTTP-only cookies with CSRF protection, and single sign-on uses PKCE. Security headers (CSP, frame and referrer policies) are set on every response.
- **Errors.** Clients see a provider's error message, but no other details of provider responses or network errors.
- **Abuse.** Requests without a valid key are refused before their body is read, headers and bodies have deadlines, and connections are capped.
- **Accounting.** Usage is accounted even when a client disconnects early: non-streamed requests run to completion, and a stream cut off before the provider reports usage is charged an estimate (logged as status 499). Options billed outside the deployment's prices (`background` responses, OpenAI's `priority` and `scale` tiers) are refused.
- **Small surface.** Few dependencies, no plugins and no code execution of any kind. AWS signing uses the official AWS SDK. The container image is distroless, runs as non-root and needs no writable file system.
- **API reference.** `/admin/docs/` loads nothing from third parties: Scalar's bundle is served by the gateway, pinned by version and hash, and its hosted features are off.
