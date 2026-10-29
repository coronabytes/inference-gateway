# Single sign-on

Admins sign in to the console with **OpenID Connect**: Keycloak, Entra ID, Google, Okta, Authentik, Kanidm or any other provider with discovery.

## Set up

1. Register a client at your identity provider with the redirect URI `https://<gateway>/admin/auth/oidc/callback`.
2. Sign in with the master key and open **Cluster → Single sign-on**. Enter the issuer, client ID and secret, the gateway's URL, the claim that carries roles (`roles` by default) and the role that grants admin access. **Test discovery** checks the issuer.
3. Enable it. The sign-in screen then offers a button with the label set there (**Single sign-on** by default).

An identity provider on a private network needs its network in `GATEWAY_UPSTREAM_ALLOWED_NETWORKS`.

## Roles

Only people with the admin role or the user role get in, so enabling single sign-on never admits everyone at your provider. The role claim may be a string, a list or an object, and nested claims work (for example `realm_access.roles`). The role is read from the ID token at every sign-in.

- **Admins** may do everything.
- **Users** (the user role, optional) see nothing until an admin adds them to projects on **Access → Users**. In their projects they manage inference and project admin keys and see the playground, request log and metrics; nothing else. The gateway enforces this on the admin API, not only in the console.
- Someone with both roles is an admin.

**Access → Users** lists everyone who has signed in; removing someone ends their sessions.

## Master key and sessions

The master key (`GATEWAY_MASTER_KEY`) works as a bearer token for scripts and as a fallback sign-in. Once single sign-on works you can remove it; for scripts, a virtual key with the admin [scope](access.md#scopes) takes its place and can be revoked or regenerated on its own.

Sessions end after 8 hours of inactivity or 24 hours after sign-in, whichever comes first, and work on every replica.
