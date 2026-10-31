import { useEffect, useState } from "react";
import { api, type ConnectRequest, type NamedItem } from "../api";
import { Banner, Button, Select } from "../components/controls";
import { ErrorBanner } from "../components/ui";

/** An MCP client's sign-in to the gateway (`#/connect/{id}`), opened by the gateway's
 *  `/oauth/authorize`: who the client is, where it returns to, and the key it will act as. */
export function Connect({ id }: { id: string }) {
  const [request, setRequest] = useState<ConnectRequest | null>(null);
  const [keys, setKeys] = useState<NamedItem[] | null>(null);
  const [keyId, setKeyId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    Promise.all([api<ConnectRequest>(`/connect/${id}`), api<NamedItem[]>("/keys/options?usable=true")])
      .then(([r, k]) => {
        setRequest(r);
        setKeys(k);
        if (k.length === 1) setKeyId(k[0].id);
      })
      .catch((e) => setError((e as Error).message));
  }, [id]);

  const answer = async (approve: boolean) => {
    setBusy(true);
    try {
      const { redirect } = approve
        ? await api<{ redirect: string }>(`/connect/${id}`, { method: "POST", json: { key_id: keyId } })
        : await api<{ redirect: string }>(`/connect/${id}/deny`, { method: "POST" });
      window.location.assign(redirect);
    } catch (e) {
      setError((e as Error).message);
      setBusy(false);
    }
  };

  const name = request?.client_name ?? request?.client_host ?? "An app";
  return (
    <div className="page connect-page">
      <section className="card form connect-card">
        <h3>Connect {name} to the gateway</h3>
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        {request && (
          <>
            <p>
              <strong>{name}</strong> asks to use the gateway's MCP servers at <span className="mono">{request.resource}</span>, as the key you pick. Its tool
              calls count against that key's limits and budget, and show in the request log.
            </p>
            <dl className="connect-facts">
              <dt>App</dt>
              <dd>
                {request.client_name ?? "No name given"} <span className="muted">from</span> <strong>{request.client_host}</strong>
                <div className="muted small mono">{request.client_id}</div>
              </dd>
              <dt>Returns to</dt>
              <dd>
                <strong>{request.redirect_host}</strong>
                <div className="muted small mono">{request.redirect_uri}</div>
              </dd>
            </dl>
            {request.redirect_local && (
              <Banner tone="warning">
                It returns to a program on your computer ({request.redirect_host}). Any program there could claim to be {name}: approve only if you
                started this sign-in yourself, just now.
              </Banner>
            )}
            {request.redirect_elsewhere && (
              <Banner tone="warning">
                It returns to {request.redirect_host}, not to {request.client_host}, which vouches for the app. Approve only if you trust both.
              </Banner>
            )}
            {keys && keys.length === 0 ? (
              <Banner tone="warning">You have no key that may call the gateway. Create one on the Access page first.</Banner>
            ) : (
              <Select
                caption="Key"
                hint="The app acts as this key: what it may use, and its limits and budget. You can revoke the app on the key's sign-ins."
                value={keyId}
                onChange={setKeyId}
                required
                options={(keys ?? []).map((k) => ({ value: k.id, label: k.name }))}
              />
            )}
            <div className="modal-foot">
              <Button onClick={() => answer(false)} disabled={busy}>
                Deny
              </Button>
              <Button variant="primary" onClick={() => answer(true)} disabled={busy || !keyId}>
                Approve
              </Button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}
