# Models and routing

A **model name** is what clients send in `model`. Behind it are one or more **deployments**, each a [credential](providers.md) plus the provider's model and the protocol to speak. Add them under **Models → Deployments**.

## Deployments

- **Mode.** A model serves chat, embeddings, rerank or decisions, and is only reachable on that mode's endpoints. The mode is chosen when a deployment is created.
- **Per-deployment settings.** Request and token rate limits, timeout, a default `max_tokens`, extra request headers, and thinking settings for translated Anthropic requests. Headers are stored in plain text, so headers that carry credentials (`Authorization`, `*-key`, `*token*` and the like) are refused: put the secret in the credential instead.
- **Tags and projects** limit which keys may use a deployment; see [Access control](access.md).
- **Renaming.** Rename a model and the auto routers and keys that use it follow along, and so do its Hub name, vendor and description.
- **Aliases.** An alias (**Add alias**, in the add button's menu) is another model name for an existing model or auto router. It follows its target when the target is renamed.

## Routing

- **Priority and weight.** The gateway uses the lowest `priority` tier first and spreads requests by `weight` within it. Deployments that are cooling down or at their own rate limits are skipped.
- **Failover.** Rate limits, server errors and timeouts move the request to the next deployment, as long as nothing has been sent to the client yet. A failing deployment is taken out of rotation for the cooldown on every replica.
- **Adaptive routing.** Every replica counts each deployment's recent calls in Valkey, a call counting half as much a minute later. A deployment whose calls fail gets less of its weight: 80% at 10% failures, 50% at 25%, and 5% from half of them on, so it is still tried and can recover. One whose time to first token is more than 1.5 times its tier's median loses weight in proportion, down to a quarter. When a whole priority tier is unhealthy, the share of its requests its health does not cover goes to the next tier. Until a deployment has 5 recent calls, it gets its full weight. The Models page marks a deployment that gets less than its weight as **Degraded**.
- **Sticky routing.** Prompt caches live with one deployment (one Azure resource, one AWS account and region), so the turns of a conversation go to the deployment that served its first turn, as long as it stays healthy, has capacity and no better priority tier is available. Spread over several deployments, each turn would miss the cache and pay full price for the whole history again. A conversation is recognised by how it starts (system prompt, tools and messages up to the first user message), by a client's `prompt_cache_key`, or by its `previous_response_id`. Only a keyed hash of this is kept in Valkey, for an hour after the conversation's last request.

Attempts, cooldown, adaptive and sticky routing are [settings](configuration.md#settings).

## Pricing

Each deployment has input, output, cache read and cache write prices per million tokens, and optionally higher prices above a long-context threshold; rerank deployments are priced per 1,000 searches, and chat deployments per 1,000 web searches the model runs. Prices drive cost reporting and budgets.

The **Pricing** tab imports a public price list, and the deployment form fills in prices from it when it recognises the model; you can always type your own. Billing uses the prices saved on the deployment.

## Model hub

The **Hub** tab lists every model the gateway serves, auto routers included, with context size, maximum output, capabilities and prices. Pick a key to see exactly the models it may use and the guardrails enforced on it. Each model can have a display name, a vendor and a description of what it is good at.

The list exports as JSON or Excel, and is available as `GET /admin/api/hub` (`key`, `mode`, `vendor` and `q`). Besides admins, a project key may call it for keys of its project, and any key for itself.
