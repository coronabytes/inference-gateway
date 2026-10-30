import { useCallback, useEffect, useState } from "react";
import { api, type AdminUser, type NamedItem, type OidcSettings, type OidcSettingsInput } from "../api";
import { Banner, Button, MultiSelect, TextInput, Toggle } from "../components/controls";
import { Badge, ConfirmDialog, ErrorBanner, IconButton, TrashIcon, type ConfirmRequest } from "../components/ui";
import { dateTime, relative } from "../format";

/** Cluster → Single sign-on: the OpenID Connect provider admins sign in with. */
export function SingleSignOn() {
  const [settings, setSettings] = useState<OidcSettings | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setSettings(await api<OidcSettings>("/settings/oidc"));
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
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {settings && <OidcForm key={settings.updated_at ?? "new"} initial={settings} onSaved={load} />}
    </div>
  );
}

/** Access → Users: everyone who signed in, admins and users, and the projects each user works in. */
export function Admins() {
  const [admins, setAdmins] = useState<AdminUser[]>([]);
  const [projects, setProjects] = useState<NamedItem[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<ConfirmRequest | null>(null);

  const load = useCallback(async () => {
    try {
      const [a, p] = await Promise.all([api<AdminUser[]>("/admins"), api<NamedItem[]>("/projects/options")]);
      setAdmins(a);
      setProjects(p);
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
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <section className="card">
        <h3>Users</h3>
        <p className="muted small">
          Everyone who signed in with single sign-on. Admins may do everything; users see only the projects added here, and there manage keys and see
          the playground, request log and metrics. Their role comes from the identity provider at each sign-in. Removing someone ends their sessions;
          they can sign in again while the single sign-on rules allow it.
        </p>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Email</th>
                <th>Role</th>
                <th>Projects</th>
                <th>Last sign-in</th>
                <th>First sign-in</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {admins.map((a) => (
                <tr key={a.id}>
                  <td>{a.name ?? <span className="muted">–</span>}</td>
                  <td>{a.email ?? <span className="muted mono">{a.subject}</span>}</td>
                  <td>{a.role === "admin" ? <Badge tone="neutral">Admin</Badge> : "User"}</td>
                  <td>
                    {a.role === "admin" ? (
                      <span className="muted small">All projects</span>
                    ) : (
                      <MultiSelect
                        caption="Projects"
                        none="None"
                        options={projects.map((p) => ({ value: p.id, label: p.name }))}
                        values={a.projects}
                        onChange={(ids) =>
                          api(`/admins/${a.id}/projects`, { method: "PUT", json: { project_ids: ids } })
                            .then(load)
                            .catch((e) => setError((e as Error).message))
                        }
                      />
                    )}
                  </td>
                  <td className="when" title={dateTime(a.last_login_at)}>{relative(a.last_login_at)}</td>
                  <td className="when">{dateTime(a.created_at)}</td>
                  <td className="actions">
                    <IconButton
                      label={`Remove ${a.email ?? a.subject}`}
                      title="Remove"
                      danger
                      onClick={() =>
                        setConfirming({
                          title: a.role === "admin" ? "Remove admin" : "Remove user",
                          message: <>Remove <strong>{a.email ?? a.subject}</strong>? Their sessions end on their next request.</>,
                          action: async () => {
                            await api(`/admins/${a.id}`, { method: "DELETE" });
                            await load();
                          },
                        })
                      }
                    >
                      <TrashIcon />
                    </IconButton>
                  </td>
                </tr>
              ))}
              {admins.length === 0 && (
                <tr>
                  <td colSpan={7} className="empty">Nobody has signed in with single sign-on yet.</td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </section>
      {confirming && <ConfirmDialog request={confirming} onClose={() => setConfirming(null)} />}
    </div>
  );
}

function OidcForm({ initial, onSaved }: { initial: OidcSettings; onSaved: () => Promise<void> }) {
  const [enabled, setEnabled] = useState(initial.enabled);
  const [displayName, setDisplayName] = useState(initial.display_name);
  const [issuer, setIssuer] = useState(initial.issuer_url);
  const [clientId, setClientId] = useState(initial.client_id);
  const [secret, setSecret] = useState("");
  const [removeSecret, setRemoveSecret] = useState(false);
  const [publicUrl, setPublicUrl] = useState(initial.public_url || window.location.origin);
  const [rolesClaim, setRolesClaim] = useState(initial.roles_claim);
  const [role, setRole] = useState(initial.allowed_role ?? "");
  const [userRole, setUserRole] = useState(initial.user_role ?? "");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const redirectUri = `${publicUrl.trim().replace(/\/+$/, "")}/admin/auth/oidc/callback`;
  const noRole = !role.trim();

  const save = async () => {
    setBusy(true);
    setNotice(null);
    try {
      const input: OidcSettingsInput = {
        enabled,
        display_name: displayName,
        issuer_url: issuer,
        client_id: clientId,
        ...(secret ? { client_secret: secret } : removeSecret ? { client_secret: "" } : {}),
        public_url: publicUrl,
        roles_claim: rolesClaim,
        allowed_role: role.trim() || null,
        user_role: userRole.trim() || null,
      };
      await api("/settings/oidc", { method: "PUT", json: input });
      setError(null);
      await onSaved();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const test = async () => {
    setNotice(null);
    try {
      const r = await api<{ ok: boolean; authorization_endpoint?: string; error?: string }>("/settings/oidc/test", { method: "POST" });
      if (r.ok) {
        setError(null);
        setNotice(`Discovery works. Sign-ins go to ${r.authorization_endpoint}.`);
      } else setError(r.error ?? "discovery failed");
    } catch (e) {
      setError((e as Error).message);
    }
  };

  return (
    <section className="card form">
      <h3>Single sign-on (OpenID Connect)</h3>
      <p className="muted small">
        Admins sign in with your identity provider. The master key keeps working as a fallback. Register a confidential (or public, PKCE) client
        at the provider with this redirect URI: <code>{redirectUri}</code>
      </p>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {notice && (
        <Banner tone="good" caption="Test" onDismiss={() => setNotice(null)}>
          {notice}
        </Banner>
      )}
      <Toggle checked={enabled} onChange={setEnabled}>
        Enable single sign-on
      </Toggle>
      <div className="row">
        <TextInput caption="Issuer URL" hint="Discovery reads {issuer}/.well-known/openid-configuration." mono value={issuer} onChange={(e) => setIssuer(e.target.value)} placeholder="e.g. https://login.example.com/realms/main" required={enabled} />
        <TextInput
          caption="Button label"
          hint="The sign-in button's whole text, e.g. “Sign in with Entra ID”."
          value={displayName}
          onChange={(e) => setDisplayName(e.target.value)}
          placeholder="Single sign-on"
        />
      </div>
      <div className="row">
        <TextInput caption="Client ID" mono value={clientId} onChange={(e) => setClientId(e.target.value)} required={enabled} />
        <TextInput
          caption="Client secret"
          hint={
            initial.client_secret_set
              ? "Stored encrypted. Leave empty to keep it, or remove it with × for a public client (PKCE only)."
              : "Leave empty for a public client (PKCE only)."
          }
          secret
          value={secret}
          onChange={(e) => {
            setSecret(e.target.value);
            setRemoveSecret(false);
          }}
          placeholder={!initial.client_secret_set ? "" : removeSecret ? "removed when you save" : "stored — leave blank to keep"}
          // × clears what was typed, and then removes the stored secret.
          {...(secret
            ? { onClear: () => setSecret(""), clearLabel: "Clear the new secret" }
            : initial.client_secret_set && !removeSecret && { onClear: () => setRemoveSecret(true), clearLabel: "Remove the stored secret" })}
        />
      </div>
      <TextInput caption="Gateway URL" hint="How browsers reach this gateway; it sets the redirect URI." mono value={publicUrl} onChange={(e) => setPublicUrl(e.target.value)} placeholder="e.g. https://gateway.example.com" />

      <fieldset>
        <legend>Who may sign in</legend>
        <p className="muted small">
          People whose ID token carries the admin role in the roles claim sign in as admins and may do everything. With the user role they sign in as users:
          they see only the projects an admin adds them to under Access → Users, and there manage keys and see the playground, request log and metrics.
        </p>
        <TextInput caption="Roles claim" hint="A string, list or object of roles. Use a dotted path for nested claims, e.g. realm_access.roles." mono value={rolesClaim} onChange={(e) => setRolesClaim(e.target.value)} placeholder="roles" />
        <div className="row">
          <TextInput caption="Admin role" hint="Matched exactly, including case." mono value={role} onChange={(e) => setRole(e.target.value)} placeholder="e.g. gateway-admin" required={enabled} />
          <TextInput
            caption="User role"
            hint="Matched exactly, including case. Leave empty to let only admins sign in. Someone with both roles is an admin."
            mono
            value={userRole}
            onChange={(e) => setUserRole(e.target.value)}
            placeholder="e.g. gateway-user"
          />
        </div>
      </fieldset>
      {enabled && noRole && <Banner tone="warning">Set the admin role; otherwise everyone at the provider could sign in.</Banner>}
      <div className="modal-foot">
        {initial.configured && (
          <Button onClick={test} disabled={busy}>
            Test discovery
          </Button>
        )}
        <Button variant="primary" onClick={save} disabled={busy || (enabled && (!issuer.trim() || !clientId.trim() || noRole))}>
          Save
        </Button>
      </div>
    </section>
  );
}
