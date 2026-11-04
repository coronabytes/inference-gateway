import { useEffect, useState } from "react";
import { api, type AdminUser, type BudgetAlerts, type SmtpSettings } from "../api";
import { MultiSelect } from "./controls";

/** Shares of the budget offered as thresholds; stored ones outside these are listed too. */
const PRESETS = [50, 75, 80, 90, 95, 99, 100];
/** What a budget without alerts offers, once someone is picked to notify. */
const DEFAULT_THRESHOLDS = [80, 99];

const sameAlerts = (a: BudgetAlerts, b: BudgetAlerts) =>
  JSON.stringify([[...a.thresholds].sort(), [...a.recipients].sort()]) ===
  JSON.stringify([[...b.thresholds].sort(), [...b.recipients].sort()]);

/**
 * The budget alerts of a project or key (`owner`, e.g. `/projects/{id}`; none for a new one),
 * and the people who can get them. Admins only: pass `enabled` false for users.
 */
export function useBudgetAlerts(owner: string | null, enabled: boolean) {
  const [stored, setStored] = useState<BudgetAlerts>({ thresholds: [], recipients: [] });
  const [alerts, setAlerts] = useState<BudgetAlerts>({ thresholds: DEFAULT_THRESHOLDS, recipients: [] });
  const [people, setPeople] = useState<AdminUser[]>([]);
  const [mailReady, setMailReady] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!enabled) return;
    Promise.all([
      api<AdminUser[]>("/admins"),
      api<SmtpSettings>("/settings/smtp"),
      owner ? api<BudgetAlerts>(`${owner}/budget-alerts`) : Promise.resolve(null),
    ])
      .then(([p, smtp, a]) => {
        setPeople(p);
        setMailReady(smtp.configured && smtp.enabled);
        if (a) {
          setStored(a);
          if (a.recipients.length) setAlerts(a);
        }
      })
      .catch((e) => setError((e as Error).message));
  }, [owner, enabled]);

  /** What to save for the project or key: nothing without a budget, and only when it changed. */
  const pending = (budget: boolean): BudgetAlerts | null => {
    if (!enabled) return null;
    const next = budget && alerts.recipients.length ? alerts : { thresholds: [], recipients: [] };
    return sameAlerts(next, stored) ? null : next;
  };

  return { alerts, setAlerts, people, mailReady, error, clearError: () => setError(null), pending };
}

/** Saves `alerts` for the project or key at `owner` (`/projects/{id}` or `/keys/{id}`). */
export async function saveBudgetAlerts(owner: string, alerts: BudgetAlerts | null) {
  if (alerts) await api(`${owner}/budget-alerts`, { method: "PUT", json: alerts });
}

/** Thresholds and the people they notify, under a budget in a project's or key's form. */
export function BudgetAlertsEditor({
  state,
  noun,
}: {
  state: ReturnType<typeof useBudgetAlerts>;
  /** "project" or "key", for the hints. */
  noun: string;
}) {
  const { alerts, setAlerts, people, mailReady } = state;
  const thresholds = [...new Set([...PRESETS, ...alerts.thresholds])].sort((a, b) => a - b);
  // People without an email cannot be notified; ones already picked stay listed, to be removed.
  const options = people
    .filter((p) => p.email || alerts.recipients.includes(p.id))
    .map((p) => ({ value: p.id, label: p.name ?? p.email ?? p.subject, note: p.name ? (p.email ?? undefined) : undefined }));
  return (
    <>
      <div className="row">
        <MultiSelect
          caption="Alert at"
          hint={`An email goes out when the ${noun}'s spend in the current budget period reaches each of these shares of the budget, once per period. Reset spend starts them over.`}
          none="No alerts"
          options={thresholds.map((t) => ({ value: String(t), label: `${t}%` }))}
          values={alerts.thresholds.map(String)}
          onChange={(v) => setAlerts({ ...alerts, thresholds: v.map(Number).sort((a, b) => a - b) })}
        />
        <MultiSelect
          caption="Notify"
          hint="People who signed in with single sign-on, at the email address their identity provider gave. Nobody: no alerts."
          none="Nobody"
          options={options}
          values={alerts.recipients}
          onChange={(recipients) => setAlerts({ ...alerts, recipients })}
        />
      </div>
      {!mailReady && alerts.recipients.length > 0 && alerts.thresholds.length > 0 && (
        <p className="muted small">Email is not set up yet: alerts go out once it is, under Cluster → Email.</p>
      )}
      {people.length > 0 && options.length === 0 && <p className="muted small">Nobody who signed in has an email address to notify.</p>}
    </>
  );
}
