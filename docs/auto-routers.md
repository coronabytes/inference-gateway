# Auto routers

An **auto router** is a model name of its own that picks the best model for each request. Add one under **Models → Deployments** with **Add router** (in the add button's menu); it needs a decision model deployment first.

A router has a decision model and a list of routes, such as Simple, Medium and Complex. Each route has a plain-language description of when to use it ("short questions and lookups", "hard reasoning, code and math") and one or more of your chat models. For every request, the decision model picks the route that fits best, and the request goes to one of that route's models at random.

- Each model in a route can have its own reasoning effort (minimal to xhigh), which replaces the client's. The same model can sit in several routes, for example with low effort for simple requests and high effort for complex ones.
- The decision model sees the conversation as plain text, trimmed to its most recent part.
- If the decision fails, the request goes to the fallback route, so routing problems never fail requests.
- **Sticky conversations** (on by default): the decision is made on a conversation's first turn, and later turns go to the same route and model. This keeps the provider's prompt cache warm and costs no further decision calls. Turn it off on the router to decide every turn anew.
- Responses say which route and model were chosen and how: `x-gateway-route-name`, `x-gateway-routed-model`, and `x-gateway-route` (`decision:<confidence>`, `sticky`, `fallback`, `pinned` or `only`).
- The request log shows a routed request with the decision and the routed call beneath it. Keys are charged once for the whole request.
- Routers serve the chat endpoints and follow the same [tag rules](access.md#tags) as deployments.
