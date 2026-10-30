# Security

How the gateway protects credentials, data and itself. To report a vulnerability, see [SECURITY.md](../SECURITY.md).

- **Secrets.** Provider secrets are only entered through the console or admin API, encrypted at rest (AES-256-GCM) and never returned. Virtual keys are stored as hashes.
- **No prompt storage by default.** Prompts and responses are not logged, in the request log or in telemetry, unless [prompt logging](#prompt-logging) is turned on for a key. Otherwise the only content kept is the encrypted, expiring history for `previous_response_id`, and none at all with `store: false`. A guardrail sends the prompt's text to its provider; the request log keeps only the kinds of things it found.
- **Outbound connections.** The gateway connects only to public addresses unless you allow more (`GATEWAY_UPSTREAM_ALLOWED_NETWORKS`), never follows redirects, and always blocks cloud metadata addresses. A Google service account file cannot send its credentials anywhere but Google.
- **Console.** Admin sessions use secure, HTTP-only cookies with CSRF protection, and single sign-on uses PKCE. Security headers (CSP, frame and referrer policies) are set on every response.
- **Errors.** Clients see a provider's error message, but no other details of provider responses or network errors.
- **Abuse.** Requests without a valid key are refused before their body is read, headers and bodies have deadlines, and connections are capped.
- **Accounting.** Usage is accounted even when a client disconnects early: non-streamed requests run to completion, and a stream cut off before the provider reports usage is charged an estimate (logged as status 499). Options billed outside the deployment's prices (`background` responses, OpenAI's `priority` and `scale` tiers) are refused.
- **Small surface.** Few dependencies, no plugins and no code execution of any kind. AWS signing uses the official AWS SDK. The container image is distroless, runs as non-root and needs no writable file system.
- **API reference.** `/admin/docs/` loads nothing from third parties: Scalar's bundle is served by the gateway, pinned by version and hash, and its hosted features are off.

## Prompt logging

For debugging, the gateway can store the full prompts and responses of chosen keys. It is meant for development and test keys while a problem is looked into, **not for production traffic**.

- **Off by default.** A project denies it (the default), allows it for the keys that turn it on, or forces it for all its keys; only admins change that. Keys outside projects follow the gateway's setting, deny or allow, never force. Logging stays on until someone turns it off: the key list marks and filters the keys that log. See [Access](access.md#projects).
- **Clients are not told.** No header says a request was logged, and clients cannot opt out. Tell the people whose traffic a logged key carries.
- **What is stored**, in Postgres (`request_bodies`), for each logged request:
  - the request as the client sent it, and as the model got it when a guardrail masked it;
  - what each guardrail found and did, **including the text it matched**, and its provider's answer; a prompt a guardrail blocked is stored too;
  - the response as the client got it, a stream put back together into one document, with tool calls and reasoning. When the gateway translates between APIs, the provider's web searches, their results and its citations, which the client's API has no place for, are kept beside it (`x_gateway_not_sent`); the client still does not get them. Guardrails do not screen responses, so they can hold personal data the model produced.
- **What is not stored:** images, files, audio, embedding vectors and encrypted provider data (replaced by their type and size), and text beyond the size limit (`prompt_log_max_kb`).
- **Not encrypted.** Unlike provider secrets and conversation history, logged bodies are plain JSON: anyone with access to the database or its backups can read them. Exclude them from backups with `pg_dump --exclude-table-data=request_bodies` if they must not be kept there.
- **Kept briefly.** `prompt_log_retention_days` (7 by default, at most 60, and never longer than the request log).
- **Search** covers the request as the client sent it, whatever the guardrails did (masked and blocked prompts are found by what they said, matched personal data included), and the response. The masked version of a request is stored, not searched.
