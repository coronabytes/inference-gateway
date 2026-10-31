import { useState } from "react";
import { api, type McpAuth, type OAuthConfig, type OAuthDiscovered } from "../api";
import { Banner, Button, Select, TextInput } from "../components/controls";

/** How the gateway signs in to an MCP server or an A2A agent, as the forms edit it. */
export interface AuthState {
  auth: McpAuth;
  header: string;
  secret: string;
  tokenUrl: string;
  authorizationUrl: string;
  clientId: string;
  scopes: string;
  resource: string;
  redirectUri: string;
  tokenAuth: "" | NonNullable<OAuthConfig["token_auth"]>;
}

export const AUTH_LABEL: Record<McpAuth, string> = {
  none: "None",
  bearer: "Bearer token",
  header: "Header",
  oauth: "OAuth: the gateway's client",
  oauth_user: "OAuth: each key signs in",
};

/** The form's state for an upstream's stored auth. */
export function authState(from?: { auth: McpAuth; auth_header?: string; oauth?: OAuthConfig }): AuthState {
  const o = from?.oauth;
  return {
    auth: from?.auth ?? "bearer",
    header: from?.auth_header ?? "x-api-key",
    secret: "",
    tokenUrl: o?.token_url ?? "",
    authorizationUrl: o?.authorization_url ?? "",
    clientId: o?.client_id ?? "",
    scopes: o?.scopes.join(" ") ?? "",
    resource: o?.resource ?? "",
    redirectUri: o?.redirect_uri ?? "",
    tokenAuth: o?.token_auth ?? "",
  };
}

const isOAuth = (a: McpAuth) => a === "oauth" || a === "oauth_user";

/** Whether the form lacks something the auth needs. `keepsSecret`: a secret is stored already. */
export function authMissing(s: AuthState, keepsSecret: boolean): boolean {
  if (s.auth === "header" && !s.header.trim()) return true;
  if ((s.auth === "bearer" || s.auth === "header") && !s.secret.trim() && !keepsSecret) return true;
  if (isOAuth(s.auth) && (!s.tokenUrl.trim() || !s.clientId.trim())) return true;
  if (s.auth === "oauth" && s.tokenAuth !== "none" && !s.secret.trim() && !keepsSecret) return true;
  return s.auth === "oauth_user" && !s.authorizationUrl.trim();
}

/** The auth as the admin API takes it. */
export function authInput(s: AuthState) {
  const t = (v: string) => v.trim() || undefined;
  return {
    auth: s.auth,
    ...(s.auth === "header" && { auth_header: s.header.trim() }),
    ...(s.auth !== "none" && s.secret.trim() && { secret: s.secret.trim() }),
    ...(isOAuth(s.auth) && {
      oauth: {
        token_url: s.tokenUrl.trim(),
        authorization_url: s.auth === "oauth_user" ? t(s.authorizationUrl) : undefined,
        client_id: s.clientId.trim(),
        scopes: s.scopes.split(/\s+/).filter(Boolean),
        resource: t(s.resource),
        redirect_uri: s.auth === "oauth_user" ? t(s.redirectUri) : undefined,
        token_auth: s.tokenAuth || undefined,
      } satisfies OAuthConfig,
    }),
  };
}

/**
 * The authentication fields of an MCP server's or A2A agent's form. `url` is what discovery starts
 * from: the MCP server's URL, or the agent's card.
 */
export function AuthFields({
  kind,
  url,
  value: s,
  onChange,
  keepsSecret,
}: {
  kind: "mcp" | "a2a";
  url: string;
  value: AuthState;
  onChange: (s: AuthState) => void;
  keepsSecret: boolean;
}) {
  const [found, setFound] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const set = (changes: Partial<AuthState>) => onChange({ ...s, ...changes });
  const what = kind === "mcp" ? "server" : "agent";

  // Fills in the client from the upstream's metadata; signing keys in registers the gateway
  // there when it has no client yet and the server offers registration.
  const discover = async () => {
    setBusy(true);
    setError(null);
    try {
      const register = s.auth === "oauth_user" && !s.clientId.trim();
      const d = await api<OAuthDiscovered>("/oauth/discover", { method: "POST", json: { kind, url: url.trim(), register } });
      set({
        tokenUrl: d.token_url ?? s.tokenUrl,
        authorizationUrl: d.authorization_url ?? s.authorizationUrl,
        scopes: s.scopes || d.scopes.join(" "),
        resource: s.resource || d.resource || "",
        ...(d.client_id && { clientId: d.client_id, secret: d.client_secret ?? "", redirectUri: d.redirect_uri ?? "", tokenAuth: d.client_secret ? "" : "none" }),
      });
      setFound(
        d.client_id
          ? "Found its authorization server and registered the gateway there."
          : register
            ? "Found its authorization server; it offers no registration, so enter a client registered there."
            : "Found its authorization server.",
      );
    } catch (e) {
      setFound(null);
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const oauth = isOAuth(s.auth);
  return (
    <>
      <Select
        caption="Authentication"
        hint={
          <ul className="hint-list">
            <li>How the gateway signs in to the {what}. Clients never see its credentials: they authenticate to the gateway with their virtual key.</li>
            <li>
              <strong>OAuth: the gateway's client</strong> gets tokens with a client registered for the gateway (client credentials).
            </li>
            <li>
              <strong>OAuth: each key signs in</strong>: each virtual key signs in to the {what} once, on the Access page, and calls it as that account.
            </li>
          </ul>
        }
        value={s.auth}
        onChange={(a) => set({ auth: a as McpAuth })}
        options={(Object.keys(AUTH_LABEL) as McpAuth[]).map((value) => ({ value, label: AUTH_LABEL[value] }))}
      />
      {s.auth === "header" && (
        <TextInput
          caption="Header"
          hint={`The header the ${what} reads its API key from.`}
          mono
          value={s.header}
          onChange={(e) => set({ header: e.target.value })}
          placeholder="x-api-key"
          required
        />
      )}
      {(s.auth === "bearer" || s.auth === "header") && (
        <TextInput
          caption={s.auth === "bearer" ? "Token" : "API key"}
          hint={s.auth === "bearer" ? "Sent as Authorization: Bearer followed by the token. Stored encrypted." : "Sent as the header's value. Stored encrypted."}
          secret
          mono
          value={s.secret}
          onChange={(e) => set({ secret: e.target.value })}
          placeholder={keepsSecret ? "unchanged" : ""}
          required={!keepsSecret}
        />
      )}
      {oauth && error && <Banner onDismiss={() => setError(null)}>{error}</Banner>}
      {oauth && found && (
        <Banner tone="good" onDismiss={() => setFound(null)}>
          {found}
        </Banner>
      )}
      {oauth && (
        <fieldset className="price-fields">
          <legend>
            OAuth client{" "}
            <Button variant="ghost" onClick={discover} disabled={busy || !url.trim()}>
              {busy ? "Looking…" : s.auth === "oauth_user" && !s.clientId.trim() ? "Discover and register" : "Discover"}
            </Button>
          </legend>
          {s.auth === "oauth_user" && (
            <TextInput
              caption="Authorization URL"
              hint="Where the person signing a key in is sent, to sign in and approve."
              mono
              value={s.authorizationUrl}
              onChange={(e) => set({ authorizationUrl: e.target.value })}
              required
            />
          )}
          <TextInput caption="Token URL" hint="Where the gateway gets and refreshes tokens." mono value={s.tokenUrl} onChange={(e) => set({ tokenUrl: e.target.value })} required />
          <TextInput caption="Client ID" mono value={s.clientId} onChange={(e) => set({ clientId: e.target.value })} required />
          <TextInput
            caption="Client secret"
            hint={s.auth === "oauth_user" ? "Leave empty for a public client, which signs in with PKCE alone. Stored encrypted." : "Stored encrypted."}
            secret
            mono
            value={s.secret}
            onChange={(e) => set({ secret: e.target.value })}
            placeholder={keepsSecret ? "unchanged" : s.auth === "oauth_user" ? "none" : ""}
            required={s.auth === "oauth" && s.tokenAuth !== "none" && !keepsSecret}
          />
          <TextInput caption="Scopes" hint="Space-separated." mono value={s.scopes} onChange={(e) => set({ scopes: e.target.value })} placeholder="optional" />
          <TextInput
            caption="Resource"
            hint={`The resource the tokens are for (RFC 8707). MCP servers expect their own URL; leave empty when the ${what} wants none.`}
            mono
            value={s.resource}
            onChange={(e) => set({ resource: e.target.value })}
            placeholder="optional"
          />
          <Select
            caption="Client authentication"
            hint="How the gateway's client proves itself at the token URL."
            value={s.tokenAuth}
            onChange={(v) => set({ tokenAuth: v as AuthState["tokenAuth"] })}
            options={[
              { value: "", label: "Automatic: Basic with a secret, else none" },
              { value: "client_secret_basic", label: "Basic (client_secret_basic)" },
              { value: "client_secret_post", label: "In the form (client_secret_post)" },
              { value: "none", label: "None: a public client" },
            ]}
          />
          {s.auth === "oauth_user" && (
            <TextInput
              caption="Redirect URI"
              hint="The address the client is registered with. Leave empty for the gateway's own: its address followed by /admin/auth/oauth/callback."
              mono
              value={s.redirectUri}
              onChange={(e) => set({ redirectUri: e.target.value })}
              placeholder={`${location.origin}/admin/auth/oauth/callback`}
            />
          )}
        </fieldset>
      )}
    </>
  );
}
