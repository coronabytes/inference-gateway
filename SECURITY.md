# Security policy

## Supported versions

Security fixes go into the latest release. Upgrade to it to get them; migrations run on startup and
upgrades need no manual steps.

## Reporting a vulnerability

Please do not open a public issue. Report it privately on GitHub instead: the repository's
**Security** tab → **Report a vulnerability**. Include what an attacker can do, the version or
commit, and the steps or a request that shows it.

You will get an answer within a week. Once a fix is released, the advisory is published with
credit to you unless you prefer otherwise.

## Scope

Of particular interest:

- Getting around a virtual key's limits: its models, tags, projects, rate limits, budget or scope.
- Reaching the admin API or console without a valid session, master key or admin key, or a
  project user or project key seeing or changing anything outside its projects.
- Reading provider credentials or the OIDC client secret, which are stored encrypted and never
  returned by the API.
- Making the gateway call addresses outside `GATEWAY_UPSTREAM_ALLOWED_NETWORKS`.
- Prompts or completions ending up in the database or logs; only request metadata is stored,
  unless prompt logging is turned on for a key (see docs/security.md).

Weaknesses in the providers themselves, or in a deployment that ignores the
[production advice](docs/production.md), are out of scope.
