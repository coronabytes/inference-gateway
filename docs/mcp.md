# MCP servers

The gateway puts [Model Context Protocol](https://modelcontextprotocol.io) servers behind the same virtual keys as the models. It signs in to each server with its own credentials, and clients never see them. Each tool call is logged, counts against the key's limits and budgets, and can be priced.

Clients reach MCP servers through the gateway in two ways:

- **`/mcp`**: one MCP server with the tools of every server the key may use, named `{server}__{tool}`, such as `github__create_issue`. Use it to connect a client once and get every tool.
- **`/mcp/{name}`**: one server, passed through as it is, with everything that server offers besides tools (resources, prompts, notifications).

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
- **Disabled servers** answer `404`, as unknown ones do, and their tools leave `/mcp`.

## In the request log

Each tool call is an entry with api `mcp`. The server's name is its model, its tool (without the server prefix) shows in the **Tools** column, and its provider is **MCP**. Its cost is the call's price. Its status says how the call went:

| Status | Meaning |
|---|---|
| `200` | The tool answered. |
| `422` | The tool ran and reported an error (`isError`). The entry's error is the tool's message. |
| `400`, `404` | The server refused the call as JSON-RPC (invalid parameters, an unknown tool, an unknown method). |
| `403` | The tool is hidden on the gateway. |
| `429` | The key or its project was over a rate limit or budget. |
| `502` | The server failed, refused the gateway's credentials, or ended its answer without a reply to the call. |
| `499` | The client went away before the tool answered. |

Where a key's prompts are [logged](monitoring.md), the call and its reply are kept like a prompt and its response. They are searchable, and the tool filter finds them. `initialize`, `tools/list` and notifications are not logged.
