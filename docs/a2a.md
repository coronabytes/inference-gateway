# A2A agents

The gateway puts [Agent2Agent (A2A)](https://a2a-protocol.org) agents behind the same virtual keys as the models. It signs in to each agent with its own credentials, which clients never see. Each message sent to an agent is logged with the state its task was left in, counts against the key's limits and budgets, and can be priced.

Agents are proxied in **A2A 1.0** and **0.3**, over **JSON-RPC** and **HTTP+JSON**. gRPC interfaces are not proxied.

## Set up

On **Tools → A2A agents → New A2A agent**:

1. **Name:** the address clients use, `/a2a/travel` for `travel`. It is lowercase letters, digits, `-` and `_`.
2. **Agent card:** where the agent's card is, such as `https://agent.example.com/.well-known/agent-card.json`. An address without a path gets `/.well-known/agent-card.json`. The card says where the agent's interfaces are.
3. **Authentication:** how the gateway signs in to the agent, for its card and its messages.
   - A bearer token, an API key in a header of your choice, or none. The secret is stored encrypted and is never shown again.
   - **OAuth:** the gateway's own client, or each key signs in to the agent. See [signing in to MCP servers and A2A agents](upstream-oauth.md). When each key signs in, the card is fetched without credentials unless the key asking has signed in.
4. **Tags** and **projects** decide which keys may use the agent, as for [deployments](access.md#tags).
5. **Price per message:** USD charged to the key for each message sent, once the agent has answered. The default is 0.
6. **Timeout:** how long an answer that is not streamed may take. The default is 600 seconds. Streams follow their task for as long as it runs.

**Try** (the flask) fetches the card and shows which interfaces the gateway proxies, at the agent and through the gateway, and the agent's skills. No message is sent.

## Connect a client

A2A clients start from the agent card. Give them the gateway's copy:

```
https://gateway.example.com/a2a/travel/.well-known/agent-card.json
```

Clients send their virtual key as a bearer token (`Authorization: Bearer sk-gw-…`), for the card and for the agent. The key needs the `inference` or `all` scope.

The card is the agent's own, with these changes:

- **Interfaces:** the agent's newest JSON-RPC interface is at `/a2a/travel`, and its newest HTTP+JSON interface at `/a2a/travel/rest`. Each keeps its A2A version.
- **Security:** the gateway's bearer key instead of the agent's own schemes.
- **Removed:** its signatures, which no longer match, and the extended card, which would list the agent's own addresses.

Labels in `x-gateway-labels` are logged with each message, as for model requests.

## What the gateway does

- **Messages pass through.** Requests and answers are forwarded as they are, streams included, with the `A2A-Version` and `A2A-Extensions` headers.
- **Tasks belong to their key.** Every key reaches the agent as the gateway, so the gateway records which key's message created each task and context. It keeps that for 30 days after each use.
  - Getting, cancelling or following a task, its push notification settings, or a message that continues a task, only work for the key that owns it. Other keys get *task not found*.
  - A message in another key's context is refused the same way. A client may start a context of its own.
  - **Listing tasks** is not offered, since it would show every key's tasks.
- **The agent's credentials stay inside.**
  - An agent that refuses the gateway's credentials (`401` or `403`) is reported as `502`. The client is not asked to sign in to the agent.
  - The client's key is never forwarded.
- **Limits and budgets:** a message is refused with `429` when the key or its project is over a rate limit or budget. Other requests are not limited.
- **Disabled agents** answer `404`, as unknown ones do.

Push notifications go from the agent straight to the address the client gave, not through the gateway.

## In the request log

Each message is an entry with api `a2a`. The agent's name is its model, and its provider is **A2A**. Its details show the task's state and context, and its cost is the message's price. Streams have a time to first event. Its status says how the exchange went:

| Status | Meaning |
|---|---|
| `200` | The agent answered. The task may be `completed`, `working`, `input-required`, `auth-required` or `canceled`. |
| `422` | The task `failed`, or the agent `rejected` it. |
| `400`, `404`, `409` | The agent refused the request as JSON-RPC (invalid, unknown task, not cancelable), or answered with that HTTP status. |
| `429` | The key or its project was over a rate limit or budget. |
| `502` | The agent failed, or refused the gateway's credentials. |
| `499` | The client went away before the agent answered. |

Where a key's prompts are [logged](monitoring.md), the message and the agent's answer are kept like a prompt and its response, streams as their list of events. Requests about tasks are not logged.
