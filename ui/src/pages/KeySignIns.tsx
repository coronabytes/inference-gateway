import { useCallback, useEffect, useState } from "react";
import { api, type ClientGrant, type KeyConnection, type VirtualKey } from "../api";
import { Button } from "../components/controls";
import { Badge, ErrorBanner, Modal } from "../components/ui";
import { dateTime } from "../format";

const KIND_LABEL = { mcp: "MCP server", a2a: "A2A agent" } as const;

/**
 * A key's sign-ins to MCP servers and A2A agents whose keys sign in on their own. Signing in sends
 * the browser to the server's sign-in; it comes back to the Access page, or to the playground with
 * the key chosen (`back`).
 */
export function KeySignIns({ keyItem: k, back, onClose }: { keyItem: Pick<VirtualKey, "id" | "name">; back?: "playground"; onClose: () => void }) {
  const [rows, setRows] = useState<KeyConnection[] | null>(null);
  /** The MCP clients signed in to the gateway as this key. */
  const [apps, setApps] = useState<ClientGrant[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [c, a] = await Promise.all([api<KeyConnection[]>(`/keys/${k.id}/connections`), api<ClientGrant[]>(`/keys/${k.id}/apps`)]);
      setRows(c);
      setApps(a);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [k.id]);

  useEffect(() => {
    load();
  }, [load]);

  const path = (c: KeyConnection) => `/keys/${k.id}/connections/${c.kind}/${c.upstream_id}`;
  const signIn = async (c: KeyConnection) => {
    setBusy(c.upstream_id);
    try {
      const { authorization_url } = await api<{ authorization_url: string }>(`${path(c)}${back ? `?back=${back}` : ""}`, { method: "POST" });
      window.location.assign(authorization_url);
    } catch (e) {
      setError((e as Error).message);
      setBusy(null);
    }
  };
  const signOut = async (c: KeyConnection) => {
    setBusy(c.upstream_id);
    try {
      await api(path(c), { method: "DELETE" });
      await load();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(null);
    }
  };

  const revoke = async (g: ClientGrant) => {
    setBusy(g.id);
    try {
      await api(`/keys/${k.id}/apps/${g.id}`, { method: "DELETE" });
      await load();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(null);
    }
  };

  return (
    <Modal title={`Sign-ins of ${k.name}`} size="wide" onClose={onClose} footer={<Button onClick={onClose}>Close</Button>}>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <p className="muted small">
        These MCP servers and A2A agents act for whoever signed in: each key signs in once, and the gateway calls them with that account for its requests. The key's
        clients never see its tokens.
      </p>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Kind</th>
              <th>Status</th>
              <th>Signed in</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows?.map((c) => (
              <tr key={`${c.kind}:${c.upstream_id}`}>
                <td className="mono">{c.name}</td>
                <td>{KIND_LABEL[c.kind]}</td>
                <td>
                  {c.connected ? (
                    <span title={c.scope ? `Scopes: ${c.scope}` : undefined}>
                      <Badge tone="good">Signed in</Badge>
                    </span>
                  ) : (
                    <Badge tone="neutral">Not signed in</Badge>
                  )}
                </td>
                <td className="muted small">
                  {c.signed_in_at && `${dateTime(c.signed_in_at)}${c.connected_by ? ` by ${c.connected_by}` : ""}`}
                  {c.connected && !c.refreshable && c.expires_at && <div>until {dateTime(c.expires_at)}, then again</div>}
                </td>
                <td className="actions">
                  {c.connected && (
                    <Button variant="ghost" disabled={busy !== null} onClick={() => signOut(c)}>
                      Sign out
                    </Button>
                  )}
                  <Button variant={c.connected ? undefined : "primary"} disabled={busy !== null} onClick={() => signIn(c)}>
                    {busy === c.upstream_id ? "Opening…" : c.connected ? "Sign in again" : "Sign in"}
                  </Button>
                </td>
              </tr>
            ))}
            {rows?.length === 0 && (
              <tr>
                <td colSpan={5} className="empty">
                  No MCP server or A2A agent this key may use signs keys in on their own.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <h4 className="dialog-subhead">Apps signed in as this key</h4>
      <p className="muted small">
        MCP clients, such as Claude or ChatGPT, that someone approved to use the gateway's MCP servers as this key. Its limits and budget apply to them.
      </p>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>App</th>
              <th>MCP endpoint</th>
              <th>Approved</th>
              <th>Last used</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {apps?.map((g) => (
              <tr key={g.id}>
                <td>
                  {g.client_name ?? g.client_id}
                  <div className="muted small mono clip" title={g.client_id}>
                    {g.client_id.replace(/^https?:\/\//, "")}
                  </div>
                </td>
                <td className="mono">{g.resource === "/" ? "every MCP endpoint" : g.resource}</td>
                <td className="muted small">
                  {dateTime(g.created_at)}
                  {g.created_by && ` by ${g.created_by}`}
                </td>
                <td className="muted small">{g.last_used_at ? dateTime(g.last_used_at) : "never"}</td>
                <td className="actions">
                  <Button variant="ghost" disabled={busy !== null} onClick={() => revoke(g)}>
                    Revoke
                  </Button>
                </td>
              </tr>
            ))}
            {apps?.length === 0 && (
              <tr>
                <td colSpan={5} className="empty">
                  No app signed in as this key. MCP clients that add the gateway with OAuth sign in through the console.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </Modal>
  );
}

/** How a sign-in that came back to the Access page or the playground went, once: `connected` or `connect_error`. */
export function takeSignInResult(): { tone: "good" | "critical"; message: string } | null {
  const [path, query] = window.location.hash.split("?");
  if (!query) return null;
  const q = new URLSearchParams(query);
  const connected = q.get("connected");
  const failed = q.get("connect_error");
  if (!connected && !failed) return null;
  window.history.replaceState(null, "", `${window.location.pathname}${path}`);
  return connected ? { tone: "good", message: `Signed in to ${connected}.` } : { tone: "critical", message: failed ?? "" };
}
