# MCP servers

The gateway puts [Model Context Protocol](https://modelcontextprotocol.io) servers behind the same virtual keys as the models. It signs in to each server with its own credentials, and clients never see them. Each tool call is logged, counts against the key's limits and budgets, and can be priced.

Clients reach MCP servers through the gateway in three ways:

- **`/mcp`**: one MCP server with the tools of every server the key may use, named `{server}__{tool}`, such as `github__create_issue`. Use it to connect a client once and get every tool.
- **`/mcp/{name}`**: one server, passed through as it is, with everything that server offers besides tools (resources, prompts, notifications).
- **In a model request**: the gateway runs the tools itself while the model answers, with any model. See [MCP tools in model requests](#mcp-tools-in-model-requests).

Servers are reached over **Streamable HTTP**. Servers that only run locally over stdio need an HTTP wrapper in front of them. The gateway does not start processes.

## Set up

On **Tools → New MCP server**:

1. **Name:** the address clients use, `/mcp/github` for `github`, and the prefix of the server's tools on `/mcp`. It is lowercase letters, digits, `-` and `_`, without `__`.
2. **URL:** the server's Streamable HTTP endpoint, such as `https://api.githubcopilot.com/mcp/`.
   - Servers on private networks need their range in `GATEWAY_UPSTREAM_ALLOWED_NETWORKS`, as for self-hosted models.
   - Cloud metadata addresses are always refused.
3. **Authentication:** how the gateway signs in to the server. The token or API key is stored encrypted and is never shown again. Leaving it empty on an edit keeps it.
   - **Bearer token** sends `Authorization: Bearer <token>`.
   - **Header** sends the key in a header of your choice, such as `x-api-key`.
   - **None** is for servers that need no credentials.
   - **OAuth**: the gateway's own client, or each key signs in to the server. See [signing in to MCP servers and A2A agents](upstream-oauth.md).
4. **Tags** and **projects** decide which keys may use the server, as for [deployments](access.md#tags):
   - A server limited to projects is only for keys in one of them.
   - A key with tags may use untagged servers and those whose tags it holds.
   - A server with neither is open to every key.
5. **Price per call:** USD charged to the key for each tool call. The default is 0.
6. **Timeout:** how long a call may take, its streamed answer included. The default is 300 seconds.

## Tools and prices

**Tools** (the flask) connects to the server as a client would and lists its tools. It works on disabled servers too. For each tool you can set:

- **On:** whether keys may call it.
  - With **Every tool**, keys may call everything the server offers, tools it adds later included, except those switched off.
  - With **Only the tools switched on**, tools the server adds later stay hidden until you switch them on.
- **Price per call:** a price of the tool's own, instead of the server's.

Hidden tools are left out of the tool lists on both endpoints. Calling one is answered like a call of an unknown tool, and logged as `403`.

A call is charged once the tool has answered, also when it reports an error, since it ran. Charges count toward the key's and its project's [budgets](access.md). The dialog also shows the configuration to give clients. Connecting is not logged or charged.

## Connect a client

Clients that read an `mcpServers` configuration, such as Claude Code, Cursor and VS Code, take the gateway's address and a virtual key:

```json
{
  "mcpServers": {
    "gateway": {
      "type": "http",
      "url": "https://gateway.example.com/mcp",
      "headers": { "Authorization": "Bearer sk-gw-…" }
    }
  }
}
```

For one server, use `https://gateway.example.com/mcp/github` as the URL instead.

The key needs the `inference` or `all` scope. As on the inference API, it also works as `x-api-key`. Labels in `x-gateway-labels` are logged with each tool call, as for model requests.

### Sign in with OAuth (Claude, ChatGPT)

Clients that add remote MCP servers by their URL alone, such as Claude and ChatGPT, sign in with OAuth instead of taking a key. Give them `https://gateway.example.com/mcp` (or `/mcp/{name}`). A person signed in to the console approves the app and picks the virtual key it acts as:

1. The client calls the gateway and gets `401` with a `WWW-Authenticate` header that names the gateway's protected resource metadata (`/.well-known/oauth-protected-resource/mcp`). The gateway is its own authorization server (`/.well-known/oauth-authorization-server`).
2. The client sends the person's browser to the gateway, which sends it on to the console. After signing in there if needed, the person sees the app's name, the host its identity comes from, where it returns to, and the MCP endpoint. They pick a key and approve, or deny.
3. The client gets an access token for that endpoint (an hour) and a refresh token (30 days, replaced on each use). A sign-in ends 90 days after it was approved however often it is refreshed, and at once when a replaced refresh token is presented again, which means someone else has it. Its tool calls count as the key's: its access, limits, budget and logging apply.

Apps signed in as a key are listed on the key's **Sign-ins** (the plug icon on the Access page), with when they were approved, by whom and last used. **Revoke** signs one out; other replicas stop taking its token within a minute.

What clients need:

- **A client ID metadata document.** The client identifies itself with the URL of a JSON document listing its name and redirect URIs (Client ID Metadata Documents, the identification MCP clients are moving to). There is no dynamic client registration: clients that only register themselves cannot sign in, and use a key as above. The gateway fetches the document from the address in `client_id`, without following redirects and at most 5 KB, and keeps it as its `Cache-Control` allows, for 1 minute to a day. Each replica keeps at most 256 documents and starts at most 60 sign-ins a minute; why a document could not be read is in the gateway log, not in the answer.
- **Public clients with PKCE.** Documents must not hold a secret; `token_endpoint_auth_method` is `none`. `private_key_jwt` is not supported yet.
- **Redirect URIs** on https, or on localhost for programs on the person's computer. The console warns about localhost (any program could claim to be the app) and about a redirect to another host than the document's.
- **`resource`:** the MCP endpoint, as `https://gateway.example.com/mcp` or `.../mcp/{name}`. A token works only there. The gateway's own address stands for all its MCP endpoints.
- **The console:** approval happens in the console, so the gateway must serve it (`GATEWAY_UI`), and the client must reach the gateway at the address people use for the console.

## `/mcp`: every server

- **The gateway answers itself.** It speaks every MCP protocol version, from 2024-11-05 to 2026-07-28, and keeps no sessions of its own, so any replica answers any request.
- **Tool lists** combine every server the key may use, without the hidden tools. A server that cannot be reached is left out, and the other servers' tools are still listed. The gateway keeps each server's list for up to a minute.
- **Tool calls** go to their server in a session the gateway keeps for the key, opened again after 10 idle minutes or when the server has forgotten it. Each key gets its own sessions, so servers that keep state per session (a browser, a shell) do not share it between keys.
- Only tools are offered here. A server's resources, prompts and its own requests to the client (sampling, elicitation) need `/mcp/{name}`.

## `/mcp/{name}`: one server

- **Messages pass through.** The gateway forwards JSON-RPC as it is, both ways: every method, notification and server-sent event stream, with the protocol's own headers (`Mcp-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, `Mcp-Param-*`).
  - It reads `tools/call` requests and their replies.
  - When the server hides tools, it also reads `tools/list` answers, and takes the hidden tools out of them.
- **Sessions are bound to the key.** The client gets the server's session id with a tag that only the key that opened the session can use. Another key presenting it, or the server's id without the tag, gets `404`, which makes clients start a new session. Any replica can forward any session.

## On both

- **The server's credentials stay inside.**
  - A server that refuses the gateway's credentials (`401` or `403`) is reported as `502`. The client is not asked to sign in to the server.
  - The client's key is never forwarded, and neither are its other headers.
- **Limits and budgets:** a tool call is refused when the key or its project is over a rate limit or budget. On `/mcp/{name}` the refusal is `429`, and on `/mcp` a JSON-RPC error with code `-32000`. Other messages are not limited.
- **Every `tools/call` needs an id** that is a string or a number, as JSON-RPC requests do. One without is answered with `-32600` and logged as `400`, rather than passed to a server that might run it unanswered.
- **Disabled servers** answer `404`, as unknown ones do, and their tools leave `/mcp`.

## MCP tools in model requests

A request to `/v1/responses`, `/v1/chat/completions` or `/v1/messages` can name gateway MCP servers among its tools. The gateway offers their tools to the model and runs the calls the model makes. This works with any model, streaming or not, and the client only sees the answer.

Responses and Chat Completions take OpenAI's `mcp` tool:

```json
{
  "model": "claude-sonnet",
  "input": "Which issues are open on the gateway repository?",
  "tools": [
    { "type": "mcp", "server_label": "github", "allowed_tools": ["search_issues"], "require_approval": "never" }
  ]
}
```

Messages takes Anthropic's MCP connector: `mcp_servers: [{"type": "url", "name": "github", "tool_configuration": {"allowed_tools": [...]}}]`, and `mcp_toolset` tools to choose a server's tools.

- **The server** is a gateway MCP server's name (`server_label`, `name`) that the key may use. A `server_url` or `url`, if given, must be the gateway's own `/mcp/{name}`. Other servers are refused, since the gateway would reach them with nothing to account for.
- **Credentials:** the gateway signs in as it does on `/mcp`, with its own credentials or the key's own sign-in. `authorization` and `headers` are refused.
- **Approval:** the gateway runs the tools without asking, so `require_approval` is `never` or left out.
- **Which tools:** `allowed_tools` narrows the server's tools further, after the server's own [tool filters](#tools-and-prices). The model sees them as function tools named `{server}__{tool}`.

How a request runs:

- **Rounds:** when the model calls gateway tools, the gateway runs the calls in parallel, gives the model their results, and asks again. This continues until the model answers, for at most 8 rounds, on the same deployment.
- **What the client gets:** one response, with the text of every round and what the gateway ran, the way the client's API returns MCP calls. The client has nothing to do for them. When the model also calls one of the client's own function tools, the response ends there and returns that call, as usual.
  - **Responses:** an `mcp_list_tools` item for each server first, then an `mcp_call` item for each call, with its `arguments`, its `output` as text and its `error`, if any. Streamed, they come as `response.output_item.added` and `response.output_item.done`, with the `response.mcp_call.*` events in between.
  - **Messages:** an `mcp_tool_use` block and an `mcp_tool_result` block for each call, as Anthropic's MCP connector returns them.
  - **Chat Completions:** no place for them. They are only logged.
  - **In a later turn:** a client may send these items and blocks back as they came. The model gets them as text, since the tools may not be offered then. A conversation continued with `previous_response_id` keeps them the same way.
- **Accounting:** the response's usage is the sum of every round. Each tool call is also an entry of its own in the request log (api `mcp`), counted against the key's limits and budgets and charged its price. A limit or budget reached mid-request becomes the tool's error, which the model gets.
- **Logging:** where the key's prompts are logged, the response is logged as the client got it. On Chat Completions, what ran is kept under `x_gateway_not_sent.mcp`.
- **Upstreams:** the request is translated even for a deployment that speaks the client's API, since the gateway drives the rounds. Tool results with images reach models that take images.

With many servers, a [tool router](#tool-routers) gives each request only the tools it needs. To try it, pick servers under **MCP** in the [playground](#in-the-playground).

## In the playground

The playground (**Models → Playground**) offers the MCP servers the chosen key may use under **MCP**. The chosen servers' tools go with every chat request, and each reply shows what ran before its text: the tools each server offered, and each call, with its arguments and result. Click one to unfold it. On Chat Completions, which has no place for the calls, they are only in the request log.

A server where each key signs in itself is marked **sign in** while the key has not signed in yet. **Sign in** in the warning above the chat starts it, and the sign-in comes back to the playground.

Tool routers are listed with the servers, marked **router**. Each reply says which of its routes the decision model picked.

## Tool routers

Every tool a model is offered costs input tokens and makes picking the right one harder. A tool router gives each request only the tools it needs: a decision model reads the conversation and picks among the router's routes, as an [auto router](auto-routers.md) picks among models.

Add one under **Tools → Tool routers**. It has a decision model and routes. Each route is a group of one MCP server's tools (all of them, or those you pick) with a description of when they are needed, such as "questions about invoices, payments and refunds".

A request names the router like an MCP server: `{"type": "mcp", "server_label": "support"}`, or `mcp_servers: [{"type": "url", "name": "support"}]` in Messages. For each request:

- **The decision:** one call to the decision model, which says for every route how sure it is that the request needs it. Routes at or above the router's **threshold** (0.5 by default) are offered, the surest first, at most **At most** of them.
- **No route needed:** the model gets none of the router's tools.
- **The decision fails:** the model gets every route's tools, so routing problems never fail a request.
- **Access:** only routes whose server the key may use are asked about. A route's server that cannot list its tools, such as one the key has not signed in to, is left out.
- **Headers:** `x-gateway-tool-router` (the router), `x-gateway-tool-routes` (the routes picked, comma-separated) and `x-gateway-tool-route` (`decision`, `fallback`, or `none` when the key may use no route).
- **Accounting:** the decision is its own entry in the request log (api `decisions`, with the decision model), charged to the key at the decision model's price.

Things to know:

- Each turn is decided anew, so a conversation's tools may change between turns. A changed tool list also changes the prompt the provider can cache.
- The decision model sees the request as sent. A Responses request continued with `previous_response_id` sends only its new turn, so the decision only sees that.
- `allowed_tools` on a router is ignored: the routes say which tools it offers.
- A router's name cannot be an MCP server's, and an MCP server a router uses cannot be deleted.
- Routers work in model requests only, not on `/mcp`, where clients list tools before there is a conversation to decide on.

## In the request log

Each tool call is an entry with api `mcp`. The server's name is its model, its tool (without the server prefix) shows in the **Tools** column, and its provider is **MCP**. Its cost is the call's price. Its status says how the call went:

| Status | Meaning |
|---|---|
| `200` | The tool answered. |
| `422` | The tool ran and reported an error (`isError`). The entry's error is the tool's message. |
| `400`, `404` | The server refused the call as JSON-RPC (invalid parameters, an unknown tool, an unknown method), or the call had no id. |
| `403` | The tool is hidden on the gateway. |
| `429` | The key or its project was over a rate limit or budget. |
| `502` | The server failed, refused the gateway's credentials, or ended its answer without a reply to the call. |
| `499` | The client went away before the tool answered. |

Where a key's prompts are [logged](monitoring.md), the call and its reply are kept like a prompt and its response. They are searchable, and the tool filter finds them. `initialize`, `tools/list` and notifications are not logged.
