# Using the API

Point any OpenAI or Anthropic SDK at the gateway and use a virtual key (`sk-gw-…`) as the API key. Keys are accepted as `Authorization: Bearer` or `x-api-key`.

```bash
# Anthropic SDKs and tools built on them, such as Claude Code
export ANTHROPIC_BASE_URL=https://gateway.example.com ANTHROPIC_API_KEY=sk-gw-...
# OpenAI SDKs (Chat Completions, Responses, embeddings)
export OPENAI_BASE_URL=https://gateway.example.com/v1 OPENAI_API_KEY=sk-gw-...
```

The `model` in a request is the model name you chose in the gateway, not the provider's.

## Endpoints

| Endpoint | For |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/responses` | OpenAI Responses, including `previous_response_id` conversations |
| `POST /v1/messages`, `POST /v1/messages/count_tokens` | Anthropic Messages |
| `POST /v1/embeddings` | OpenAI embeddings |
| `POST /v1/rerank` | Rerank (Cohere's API; also `/v2/rerank` and `/rerank`) |
| `POST /v1/decisions`, `POST /v1/systemone` | Decision models |
| `GET /v1/models` | The models this key may use |

When client and provider speak the same protocol, requests pass through untouched, so provider-specific features keep working. Otherwise the gateway translates text, images, tools, reasoning, prompt caching, structured output, web search and usage between them.

Web search is each API's own: the `web_search` tool (Responses), `web_search_options` (Chat Completions, also Google Search grounding for Gemini) and the `web_search_*` server tool (Messages). The gateway sends whichever the deployment's provider takes; Bedrock has none and refuses it. The answer comes back as text, with its citations only when client and provider speak the same protocol. Each search the model runs is billed at the deployment's price per 1,000 web searches.

## API reference

The whole API, inference and admin, is described as OpenAPI 3.1 at `/admin/openapi.json` and can be browsed and tried out at `/admin/docs/` (the book icon in the console). Both are for people signed in to the console, and for the master key and admin and project keys (as `Authorization: Bearer`); others get `401`. The **API reference** [setting](configuration.md#settings) turns both off.

Everything in the console is also available through the admin API (`/admin/api/…`), for example:

```bash
H="authorization: Bearer $GATEWAY_MASTER_KEY"
CRED=$(curl -s -X POST localhost:8080/admin/api/credentials -H "$H" -H 'content-type: application/json' \
  -d '{"name": "bedrock-eu", "provider": "bedrock", "settings": {"region": "eu-central-1"}, "secrets": {}}' \
  | jq -r .id)   # no secrets: use the AWS credential chain
curl -X POST localhost:8080/admin/api/deployments -H "$H" -H 'content-type: application/json' -d '{
  "model_name": "assistant", "provider": "bedrock", "upstream_model": "<bedrock model id>",
  "credential_id": "'"$CRED"'", "tags": ["eu"]
}'
```

## Labels

Label each request, for example with the tenant or feature it serves, then filter the request log and metrics by those labels. Send them in the `x-gateway-labels` header as comma-separated `key=value` pairs:

```bash
curl https://gateway.example.com/v1/chat/completions -H "authorization: Bearer $KEY" \
  -H 'x-gateway-labels: tenant=acme,feature=search' -d '…'
```

- Every inference endpoint takes labels. Most SDKs can send a header with every request (`default_headers` in OpenAI's SDKs, `ANTHROPIC_CUSTOM_HEADERS` for Claude Code).
- Up to 5 labels per request. Keys are letters, digits, `_`, `-` and `.`; values are printable ASCII without commas; each shorter than 128 characters. Invalid labels are refused with `400` before anything is sent to a provider.
- Labels are stored with the request, indexed, and kept as long as the request log. They are not sent to providers.
- On **Metrics** and **Logs → Request log**, the **Labels** filter shows only requests with all of the given labels. In the admin API, `GET /admin/api/logs` and `GET /admin/api/metrics` take `labels=tenant=acme,feature=search`.

## Guardrails per request

A client can ask for [guardrails](guardrails.md) by name, comma-separated, on top of those its key enforces:

```bash
curl https://gateway.example.com/v1/chat/completions -H "authorization: Bearer $KEY" \
  -H 'x-gateway-guardrails: pii-and-prompt-attacks' -d '…'
```

## Conversations

Responses API clients can continue a conversation with `previous_response_id` on any model, not only on providers that store conversations themselves. For those that do not, the gateway keeps the history (encrypted, until the conversation goes unused for 15 minutes) and prefers the deployment that served the previous turn; for those that do, the conversation continues on the deployment that holds it. Conversations belong to the key that started them, and `store: false` turns this off. Each key may keep up to 64 MB of history; turns beyond it are answered but cannot be continued. Both limits are [settings](configuration.md#settings).

## Embeddings

Embeddings use the OpenAI request format on every provider, including `dimensions` and base64 output. Where a provider embeds one text per call, the gateway sends a batch in parallel and returns one response in input order.

## Rerank

Rerank models order documents by relevance to a query, for search and retrieval-augmented generation. Clients send Cohere's request (`query`, up to 1,000 `documents` as strings or `{ "text": … }`, optional `top_n` and `return_documents`) and get Cohere's v2 answer: the documents, most relevant first, each with its `index` and `relevance_score`. Every provider answers in that format:

- **Azure AI Foundry:** Cohere rerank models.
- **Amazon Bedrock:** Amazon Rerank and Cohere Rerank, through Bedrock's Rerank API. A credential with an endpoint override needs the Agent Runtime's endpoint (`bedrock-agent-runtime`).
- **OpenAI-compatible:** any server with Cohere's rerank API at `{base}/rerank`: Cohere itself (`https://api.cohere.com/v2`), Jina, vLLM and others.

Rerank models are priced per 1,000 searches (one query over up to 100 documents), and optionally per input token.

## Decision models

Decision models answer questions about an input with calibrated probabilities instead of generated text: yes/no, multiple choice, scores and bounding boxes. The gateway accepts both the OpenAI Decisions format (`/v1/decisions`) and System One (`/v1/systemone`), and checks requests before any provider is called. [Auto routers](auto-routers.md) use decision models to pick their route.

## Response headers

| Header | Meaning |
|---|---|
| `x-gateway-route-name`, `x-gateway-routed-model`, `x-gateway-route` | Which route and model an [auto router](auto-routers.md) chose, and how. |
| `x-gateway-guardrail` | What [guardrails](guardrails.md) did: `passed`, `flagged`, `masked`. |
| `x-gateway-guardrail-prompt` | With `masked`: the prompt's last text as the model got it, base64. |

## Errors

| Status | When |
|---|---|
| `400` | Invalid request, labels or guardrail name; a prompt blocked by a guardrail (`code: content_filter`). |
| `401` | Missing or unknown key. |
| `403` | The key may not use this model or API. |
| `429` | Rate limit or budget reached; the message says when a budget resets. |
| `503` | A guardrail could not be reached. |

Clients see a provider's own error message, but no other details of provider responses or network errors.

## Not yet supported

- OpenAI built-in tools when the request is translated to another provider (rejected with a clear error).
- Retrieving or deleting stored responses (`GET`/`DELETE /v1/responses/{id}`).
- Options billed outside a deployment's prices (`background` responses, OpenAI's `priority` and `scale` tiers) are refused.
