# Signing in to MCP servers and A2A agents

Many hosted MCP servers and A2A agents use OAuth rather than a static API key. The gateway supports two ways to use it, chosen under **Authentication** in the server's or agent's form:

- **OAuth: the gateway's client.** The gateway gets tokens with a client registered for it (the client credentials grant). Every key calls the upstream as that one account. Use this for service accounts and machine-to-machine APIs.
- **OAuth: each key signs in.** Each virtual key signs in to the upstream once, and the gateway then calls it as that account for that key's requests. Use this for servers that act for a person, such as GitHub, Atlassian or Linear.

Clients keep authenticating to the gateway with their virtual key. They never see the upstream's tokens.

## Set up the client

1. Choose the OAuth authentication and press **Discover**. The gateway finds the authorization server and fills in the form:
   - For an MCP server, from its protected resource metadata (RFC 9728).
   - For an A2A agent, from the OAuth scheme on its agent card.
   - Then from the authorization server's own metadata (RFC 8414, or OpenID Connect discovery).
2. With **each key signs in** and no client id yet, the button reads **Discover and register**. The gateway registers itself at the authorization server as a public client (RFC 7591), with PKCE and no secret, where the server offers registration. Otherwise, register a client there yourself and enter its id and secret.
3. Check the fields:
   - **Token URL** and, for signing keys in, **Authorization URL**.
   - **Client ID** and **Client secret**. The secret is stored encrypted, and a public client has none.
   - **Scopes**, separated by spaces.
   - **Resource:** the RFC 8707 resource the tokens are for. MCP servers expect their own URL, which discovery fills in.
   - **Client authentication:** Basic, in the form, or none.
   - **Redirect URI** (signing keys in only): the address registered with the client. Leave it empty for the gateway's own address followed by `/admin/auth/oauth/callback`.

The redirect URI must reach the gateway's console address. Behind a proxy, the proxy must pass the original host on (`Host` or `X-Forwarded-Host`) and the scheme (`X-Forwarded-Proto`).

## Sign keys in

On **Access**, the plug icon on a key opens its **Sign-ins**: the MCP servers and A2A agents the key may use whose keys sign in on their own.

- **Sign in** sends the browser to the upstream's sign-in. After approving, it comes back to the Access page.
- **Sign out** deletes the key's tokens.

Admins can sign in any key. Users can sign in their projects' keys. The sign-in only completes in the browser that started it, so a link cannot sign someone's key in to another account. The audit log records each sign-in and sign-out.

## How tokens are kept

- **The gateway's client:** each replica gets its own token and keeps it in memory until a minute before it expires.
- **Each key's sign-in:** the tokens are stored encrypted in Postgres, one set per key and upstream.
  - Before a token expires, the gateway refreshes it, one replica at a time, since authorization servers may issue a new refresh token each time.
  - A replica keeps a key's token in memory for at most five minutes. After a sign-out, other replicas stop using the token within that time.
  - The tokens are deleted with the key or the upstream, and re-encrypted by `rotate-key`.
- **A refused token** (`401`) is renewed and the request is tried once more.
- **When the sign-in no longer works,** for example when the refresh token was revoked, the key counts as not signed in.

A key that has not signed in to an upstream gets `403` on `/mcp/{name}` and `/a2a/{name}`, with a message pointing to its Sign-ins. On `/mcp`, that server's tools are left out until the key signs in.
