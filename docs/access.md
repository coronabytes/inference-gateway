# Keys and access control

## Virtual keys

Create a **virtual key** per app or team under **Access → Keys**. The secret is shown once; the gateway stores only a hash. **Regenerate** issues a new secret immediately, and the key keeps its settings, spend and history.

- **Rate limits:** requests and tokens per minute, enforced across all replicas.
- **Budgets:** an amount in USD per day, week (from Monday) or month (from the 1st), in UTC. Once spent, requests are refused with `429` and a message saying when the budget resets. Spend can be reset from the console.
- **Expiry:** a date after which the key stops working.
- **Models and tags:** which models the key may use (see [Tags](#tags)).
- **Guardrails:** [guardrails](guardrails.md) every request of the key goes through.

### Scopes

| Scope | May call |
|---|---|
| **Inference** (default) | Models. |
| **Admin** | The admin API (`/admin/api/…`), with the same rights as the master key. For scripts and automation. |
| **Inference + admin** | Both. |
| **Project admin** | The admin API with the rights of a [user](single-sign-on.md#roles) in the key's project: its keys, playground, request log and metrics. |

Admin and inference + admin keys stay outside projects. A key in a project is an inference or project admin key. Keys without the inference scope get `403` from the inference API, inference keys `403` from the admin API. Tags, rate limits and budgets apply to inference calls only.

## Projects

**Projects** (Access → Projects) group keys by team, app or customer.

- Turning a project off blocks all its keys at once, until it is turned on again.
- A project can have its own **budget** per day, week or month for all its keys together, on top of each key's own budget. Once spent, the project's keys get `429` until it resets.
- Guardrails set on a project apply to every key in it.
- Each request records the project its key was in, so when a key moves, what it spent stays with the old project. Deleting a project deletes its keys too.
- [Users](single-sign-on.md#roles) see only the projects an admin adds them to.

## Tags

Tags are labels you put on deployments and auto routers, such as `eu`, `anthropic`, `cheap` or `expensive`. Keys pick from the tags in use; new tags are only created on models.

- A key **without tags** can use every model (except deployments dedicated to other projects, below).
- A key **with tags** can use untagged deployments and every deployment whose tags it **all** holds. A deployment tagged `eu` and `expensive` needs a key with both tags.
- This is checked per deployment. If a model has one deployment tagged `eu` and another tagged `us`, an `eu` key is only ever served by the EU deployment.
- Models a key cannot use answer `403` and are left out of its `GET /v1/models`.
- Auto routers follow the same rule, and only choose among the routes the key may use.

## Dedicated deployments

A deployment can be limited to one or more **projects** (Projects in the deployment's form). Only keys in those projects can use it, whatever tags other keys hold; keys without a project never reach it. Within the projects, the deployment's tags still apply.

- Like tags, this is checked per deployment: a model with a dedicated deployment and an open one serves a project's keys from both and everyone else from the open one only.
- A project cannot be deleted while deployments are limited to it.
