import { useCallback, useEffect, useState } from "react";
import { api, type GatewaySettings as Settings, type GatewaySettingsView } from "../api";
import { Button, TextInput, Toggle } from "../components/controls";
import { ErrorBanner } from "../components/ui";
import { dateTime, relative } from "../format";

type NumberKey = { [K in keyof Settings]: Settings[K] extends number ? K : never }[keyof Settings];
type ToggleKey = { [K in keyof Settings]: Settings[K] extends boolean ? K : never }[keyof Settings];
type Item =
  | { key: NumberKey; caption: string; hint: string; valid?: (n: number) => boolean; rule?: string }
  | { key: ToggleKey; caption: string; hint: string; toggle: true };

/** Log retention: kept forever (0) or 7 to 1080 days, as the gateway requires. */
const retention = { valid: (n: number) => n === 0 || (n >= 7 && n <= 1080), rule: "0 (kept forever) or 7 to 1080" };

/** The settings by group, each with its caption and what it does. Ranges are checked by the gateway, retention also here. */
const GROUPS: { title: string; items: Item[] }[] = [
  {
    title: "Routing",
    items: [
      { key: "max_attempts", caption: "Attempts per request", hint: "Deployments tried per request on rate limits, server errors and timeouts, 1 to 10." },
      { key: "cooldown_secs", caption: "Cooldown after a failure (s)", hint: "How long a failing deployment is skipped." },
      {
        key: "reload_secs",
        caption: "Full reload every (s)",
        hint: "Changes reach every replica at once over Valkey. This periodic reload from Postgres catches any a replica missed, at least every 5 s.",
      },
      {
        key: "adaptive_routing",
        caption: "Adaptive routing",
        hint: "Deployments get less traffic while their recent calls fail or are slow. Off, requests spread by the configured weights only.",
        toggle: true,
      },
      {
        key: "sticky_routing",
        caption: "Sticky routing",
        hint: "Conversations stay on the deployment, and on an auto router the route, that served them. Off, every request is balanced on its own.",
        toggle: true,
      },
    ],
  },
  {
    title: "Retention",
    items: [
      {
        key: "request_log_retention_days",
        caption: "Request log (days)",
        hint: "How long request logs, and so metrics, are kept: 7 to 1080 days, or 0 to keep them forever. The request log cannot be turned off.",
        ...retention,
      },
      {
        key: "audit_log_retention_days",
        caption: "Audit log (days)",
        hint: "How long audit log entries are kept, independently of the request log: 7 to 1080 days, or 0 to keep them forever. The audit log cannot be turned off.",
        ...retention,
      },
      {
        key: "response_ttl_minutes",
        caption: "Conversation state, idle (min)",
        hint: "How long a previous_response_id conversation can go unused and still be continued. Each continuation starts the time again.",
      },
      {
        key: "conversation_quota_mb",
        caption: "Conversation state per key (MB)",
        hint: "History one key may keep in Valkey: the size of its conversations that have not expired. Turns beyond it are answered but cannot be continued.",
      },
    ],
  },
  {
    title: "Client limits",
    items: [
      { key: "max_body_mb", caption: "Request body (MB)", hint: "The largest request body." },
      { key: "header_timeout_secs", caption: "Header timeout (s)", hint: "Time a client has to send its request headers." },
      { key: "body_timeout_secs", caption: "Body timeout (s)", hint: "Time a client has to send its whole request body. Responses are not limited." },
      { key: "max_connections", caption: "Client connections", hint: "Open client connections on each replica; more wait in the queue." },
    ],
  },
  {
    title: "API reference",
    items: [
      {
        key: "api_docs",
        caption: "API reference",
        hint: "The OpenAPI document at /admin/openapi.json and the reference at /admin/docs/, for signed-in people and admin and project keys.",
        toggle: true,
      },
    ],
  },
];

const isToggle = (i: Item): i is Extract<Item, { toggle: true }> => "toggle" in i;

/** Numbers as typed, so a field can be emptied while editing. */
type Draft = { [K in keyof Settings]: Settings[K] extends number ? string : Settings[K] };

const toDraft = (s: Settings) => Object.fromEntries(Object.entries(s).map(([k, v]) => [k, typeof v === "number" ? String(v) : v])) as Draft;

/** Cluster → Settings: the gateway settings every replica applies, without a restart. */
export function GatewaySettings() {
  const [view, setView] = useState<GatewaySettingsView | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setView(await api<GatewaySettingsView>("/settings"));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  return (
    <div className="page">
      <ErrorBanner error={error} />
      {view && <SettingsForm key={view.updated_at ?? "defaults"} view={view} onSaved={setView} />}
    </div>
  );
}

function SettingsForm({ view, onSaved }: { view: GatewaySettingsView; onSaved: (v: GatewaySettingsView) => void }) {
  const [draft, setDraft] = useState<Draft>(() => toDraft(view.settings));
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const saved = toDraft(view.settings);
  const defaults = toDraft(view.defaults);
  const same = (a: Draft, b: Draft) => (Object.keys(a) as (keyof Draft)[]).every((k) => String(a[k]).trim() === String(b[k]).trim());
  const dirty = !same(draft, saved);
  const numbers = GROUPS.flatMap((g) => g.items).filter((i) => !isToggle(i));
  const invalid = numbers.filter((i) => !/^\d+$/.test(String(draft[i.key]).trim())).map((i) => i.caption);
  const outOfRange = numbers.filter((i) => !invalid.includes(i.caption) && i.valid && !i.valid(Number(String(draft[i.key]).trim())));

  const save = async () => {
    setBusy(true);
    try {
      const settings = Object.fromEntries(Object.entries(draft).map(([k, v]) => [k, typeof v === "string" ? Number(v.trim()) : v])) as unknown as Settings;
      onSaved(await api<GatewaySettingsView>("/settings", { method: "PUT", json: settings }));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card form">
      <h3>Gateway settings</h3>
      <p className="muted small">
        Saved in Postgres and applied on every replica within seconds, without a restart. Timeouts and the body limit apply to client connections opened
        after the change.{" "}
        {view.updated_at ? <span title={dateTime(view.updated_at)}>Last saved {relative(view.updated_at)}.</span> : "All settings are at their defaults."}
      </p>
      <ErrorBanner error={error} />
      {GROUPS.map((g) => (
        <fieldset key={g.title}>
          <legend>{g.title}</legend>
          <div className="row">
            {g.items.map((i) => {
              const hint = `${i.hint} Default: ${isToggle(i) ? (defaults[i.key] ? "on" : "off") : defaults[i.key]}.`;
              return isToggle(i) ? (
                <Toggle key={i.key} caption={i.caption} hint={hint} checked={draft[i.key]} onChange={(v: boolean) => setDraft({ ...draft, [i.key]: v })} />
              ) : (
                <TextInput
                  key={i.key}
                  caption={i.caption}
                  hint={hint}
                  inputMode="numeric"
                  value={draft[i.key]}
                  onChange={(e) => setDraft({ ...draft, [i.key]: e.target.value })}
                  required
                />
              );
            })}
          </div>
        </fieldset>
      ))}
      {invalid.length > 0 && <div className="banner">Enter a whole number for {invalid.join(", ")}.</div>}
      {outOfRange.map((i) => (
        <div key={i.key} className="banner">
          {i.caption} must be {i.rule}.
        </div>
      ))}
      <div className="modal-foot">
        <Button variant="ghost" onClick={() => setDraft(defaults)} disabled={busy || same(draft, defaults)}>
          Reset to defaults
        </Button>
        <Button onClick={() => setDraft(saved)} disabled={busy || !dirty}>
          Discard changes
        </Button>
        <Button variant="primary" onClick={save} disabled={busy || !dirty || invalid.length > 0 || outOfRange.length > 0}>
          Save
        </Button>
      </div>
    </section>
  );
}
