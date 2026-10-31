# MCP servers

The gateway puts [Model Context Protocol](https://modelcontextprotocol.io) servers behind the same virtual keys as the models. Clients reach every server at `/mcp/{name}` with their key. The gateway signs in to the server with its own credentials, and clients never see them. Each tool call is logged and counts against the key's limits and budgets.

Servers are proxied over **Streamable HTTP**. Servers that only run locally over stdio need an HTTP wrapper in front of them. The gateway does not start processes.

## Set up

On **Tools → New MCP server**:

1. **Name:** the address clients use, `/mcp/github` for `github`. It is lowercase letters, digits, `-` and `_`.
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
5. **Timeout:** how long a call may take, its streamed answer included. The default is 300 seconds.

**Try** (the flask) connects to the server as a client would and lists its tools. It also works on a disabled server and shows the configuration to give clients. Nothing is logged or charged.

## Connect a client

Clients that read an `mcpServers` configuration, such as Claude Code, Cursor and VS Code, take the gateway's address and a virtual key:

```json
{
  "mcpServers": {
    "github": {
      "type": "http",
      "url": "https://gateway.example.com/mcp/github",
      "headers": { "Authorization": "Bearer sk-gw-…" }
    }
  }
}
```

The key needs the `inference` or `all` scope. Like the inference API, the MCP endpoint also takes it as `x-api-key`. Labels in `x-gateway-labels` are logged with each tool call, as for model requests.

## What the gateway does

- **Messages pass through.** The gateway forwards JSON-RPC as it is, both ways: every method, notification and server-sent event stream. It reads only `tools/call` requests and their replies.
- **Sessions are bound to the key.** The client gets the server's session id with a tag that only the key that opened the session can use. Another key presenting it, or the server's id without the tag, gets `404`, which makes clients start a new session. Any replica can forward any session.
- **The server's credentials stay inside.**
  - A server that refuses the gateway's credentials (`401` or `403`) is reported as `502`. The client is not asked to sign in to the server.
  - The client's key is never forwarded, and neither are any other headers apart from the MCP protocol's own.
- **Limits and budgets:** a tool call is refused with `429` when the key or its project is over a rate limit or budget. Other messages are not limited.
- **Disabled servers** answer `404`, as unknown ones do.

## In the request log

Each tool call is an entry with api `mcp`, the server's name as its model, the tool in the **Tools** column and **MCP** as its provider. Its status says how the call went:

| Status | Meaning |
|---|---|
| `200` | The tool answered. |
| `422` | The tool ran and reported an error (`isError`). The entry's error is the tool's message. |
| `400`, `404` | The server refused the call as JSON-RPC (invalid parameters, an unknown tool, an unknown method). |
| `502` | The server failed, refused the gateway's credentials, or ended its answer without a reply to the call. |
| `499` | The client went away before the tool answered. |

Where a key's prompts are [logged](monitoring.md), the call and its reply are kept like a prompt and its response. They are searchable, and the tool filter finds them. Tool calls cost nothing in this version. `initialize`, `tools/list` and notifications are not logged.
