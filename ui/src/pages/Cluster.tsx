import { useCallback, useEffect, useState } from "react";
import { api, type Cluster as ClusterData, type InstanceSettings } from "../api";
import { Tabs, useHashTab } from "../components/controls";
import { Badge, ErrorBanner } from "../components/ui";
import { dateTime, duration, relative } from "../format";
import { ConfigTransfer } from "./ConfigTransfer";
import { GatewaySettings } from "./GatewaySettings";
import { SingleSignOn } from "./Settings";

// "settings" is the single sign-on tab, which used to hold the admins and export and import too; old links keep working.
// The gateway settings, formerly environment variables, are "gateway".
// The users ("admins") are on the Access page now, and the request and audit logs on the Logs page.
const TABS = ["instances", "gateway", "settings", "transfer"] as const;

export function Cluster() {
  const [tab, setTab] = useHashTab("cluster", TABS);
  return (
    <div className="page">
      <Tabs
        label="Cluster"
        options={[
          { value: "instances", label: "Instances" },
          { value: "gateway", label: "Settings" },
          { value: "settings", label: "Single sign-on" },
          { value: "transfer", label: "Backup & Restore" },
        ]}
        value={tab}
        onChange={setTab}
      />
      {tab === "gateway" ? (
        <GatewaySettings />
      ) : tab === "settings" ? (
        <SingleSignOn />
      ) : tab === "transfer" ? (
        <div className="page">
          <ConfigTransfer />
        </div>
      ) : (
        <Instances />
      )}
    </div>
  );
}

function Instances() {
  const [data, setData] = useState<ClusterData | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setData(await api<ClusterData>("/cluster"));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 5_000);
    return () => clearInterval(t);
  }, [load]);

  return (
    <>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {data && (
        <>
          {/* Only trouble is worth a line. */}
          {(!data.postgres_ok || !data.valkey_ok) && (
            <div className="filters">
              {!data.postgres_ok && <Badge tone="critical">Postgres unreachable</Badge>}
              {!data.valkey_ok && <Badge tone="critical">Valkey unreachable — limits and sync degraded</Badge>}
            </div>
          )}
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Instance</th>
                  <th>Version</th>
                  <th>Uptime</th>
                  <th className="num">Deployments</th>
                  <th className="num">Keys</th>
                  <th>Config loaded</th>
                  <th>Last heartbeat</th>
                </tr>
              </thead>
              <tbody>
                {data.instances
                  .sort((a, b) => a.id.localeCompare(b.id))
                  .map((i) => (
                    <tr key={i.id}>
                      <td className="mono">
                        {i.id}
                        {i.id === data.this_instance && <span className="muted small"> (serving this page)</span>}
                      </td>
                      <td>{i.version}</td>
                      <td>{duration(i.uptime_secs)}</td>
                      <td className="num">{i.deployments}</td>
                      <td className="num">{i.keys}</td>
                      <td>
                        {dateTime(i.registry_loaded_at)}
                        {i.registry_errors > 0 && <> <Badge tone="warning">{i.registry_errors} misconfigured</Badge></>}
                      </td>
                      <td className="when">{relative(i.last_seen)}</td>
                    </tr>
                  ))}
                {data.instances.length === 0 && (
                  <tr>
                    <td colSpan={7} className="empty">No heartbeats in Valkey.</td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
          <Environment instance={data.this_instance} settings={data.settings} />
        </>
      )}
    </>
  );
}

const onOff = (b: boolean) => (b ? "On" : "Off");

/** Each group of environment settings: a label, the value as shown, and the variable that sets it. */
function groups(s: InstanceSettings): { title: string; rows: [string, string, string][] }[] {
  return [
    {
      title: "Access and network",
      rows: [
        ["Listens on", s.listen, "GATEWAY_LISTEN"],
        ["Sign-in with the master key", s.master_key ? "Allowed" : "Off", "GATEWAY_MASTER_KEY"],
        ["Secure session cookie", onOff(s.cookie_secure), "GATEWAY_COOKIE_SECURE"],
        ["Console", s.console ? "Served" : "Not served", "GATEWAY_UI"],
        ["Private upstream networks", s.upstream_allowed_networks.join(", ") || "None", "GATEWAY_UPSTREAM_ALLOWED_NETWORKS"],
      ],
    },
    {
      title: "Storage",
      rows: [
        ["Database connections", String(s.database_max_connections), "DATABASE_MAX_CONNECTIONS"],
        ["Valkey cluster", onOff(s.valkey_cluster), "VALKEY_CLUSTER"],
      ],
    },
  ];
}

/** The answering instance's environment. Each instance reads its own, so others may differ. */
function Environment({ instance, settings }: { instance: string; settings: InstanceSettings }) {
  return (
    <section className="card instance-settings">
      <h3>
        Environment of <span className="mono">{instance}</span>
      </h3>
      <p className="muted small">
        Read from this instance's environment at startup; other instances may differ. Secrets are not shown. Everything else is under{" "}
        <a href="#/cluster/gateway">Settings</a>, for every instance.
      </p>
      <div className="settings-grid">
        {groups(settings).map((g) => (
          <div key={g.title}>
            <h4>{g.title}</h4>
            <dl>
              {g.rows.map(([label, value, env]) => (
                <div key={env} className="setting" title={env}>
                  <dt>{label}</dt>
                  <dd>{value}</dd>
                </div>
              ))}
            </dl>
          </div>
        ))}
      </div>
    </section>
  );
}
