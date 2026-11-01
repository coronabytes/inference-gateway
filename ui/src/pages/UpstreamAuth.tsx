import { type ReactNode, useEffect, useRef, useState } from "react";
import { api, type McpAuth, type OAuthConfig, type OAuthDiscovered, type SignInBy } from "../api";
import { Banner, Button, Segmented, Select, TextInput } from "../components/controls";
import { Section } from "../components/ui";

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
  /** For `oauth_user`: what a key's sign-ins are kept per: the key, each tenant, or each tenant's user. */
  signInBy: SignInPer;
  /** For `oauth_user`: days a sign-in is kept unused, as typed. */
  retentionDays: string;
  /** No client yet, but the authorization server offers registration: the gateway registers on save. */
  registerOnSave: boolean;
}

export const AUTH_LABEL: Record<McpAuth, string> = {
  none: "None",
  bearer: "Bearer token",
  header: "Header",
  oauth: "OAuth: the gateway's client",
  oauth_user: "OAuth: each key signs in",
};

/** What a key's sign-ins to an `oauth_user` upstream are kept per, as the form picks it. */
export type SignInPer = "key" | "tenant" | "user";

const SIGN_IN_BY: Record<Exclude<SignInPer, "key">, SignInBy> = { tenant: ["tenant"], user: ["tenant", "user"] };

export const SIGN_IN_PER_LABEL: Record<SignInPer, string> = { key: "Key", tenant: "Tenant", user: "Tenant and user" };

/** The form's choice for a stored `sign_in_by`. */
export function signInPer(by?: readonly string[]): SignInPer {
  return by?.length === 2 ? "user" : by?.length === 1 ? "tenant" : "key";
}

/** Days a sign-in is kept unused when the upstream does not say, and the range it may say. */
const SIGN_IN_RETENTION_DAYS = 7;
const RETENTION = { min: 1, max: 90 };

/** Why the typed retention would be refused, or null. */
function retentionError(days: string): string | null {
  const n = Number(days.trim());
  return Number.isInteger(n) && n >= RETENTION.min && n <= RETENTION.max ? null : `Between ${RETENTION.min} and ${RETENTION.max} days.`;
}

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
    scopes: o?.scopes?.join(" ") ?? "",
    resource: o?.resource ?? "",
    redirectUri: o?.redirect_uri ?? "",
    tokenAuth: o?.token_auth ?? "",
    signInBy: signInPer(o?.sign_in_by),
    retentionDays: String(o?.sign_in_retention_days ?? SIGN_IN_RETENTION_DAYS),
    registerOnSave: false,
  };
}

const isOAuth = (a: McpAuth) => a === "oauth" || a === "oauth_user";
/** Whether the gateway may get itself a client while discovering: only keys signing in use one. */
const getsClient = (s: AuthState) => s.auth === "oauth_user" && !s.clientId.trim();

/** Whether the form lacks something the auth needs. `keepsSecret`: a secret is stored already. */
export function authMissing(s: AuthState, keepsSecret: boolean): boolean {
  if (s.auth === "header" && !s.header.trim()) return true;
  if ((s.auth === "bearer" || s.auth === "header") && !s.secret.trim() && !keepsSecret) return true;
  if (isOAuth(s.auth) && (!s.tokenUrl.trim() || (!s.clientId.trim() && !(s.auth === "oauth_user" && s.registerOnSave)))) return true;
  if (s.auth === "oauth" && s.tokenAuth !== "none" && !s.secret.trim() && !keepsSecret) return true;
  return s.auth === "oauth_user" && (!s.authorizationUrl.trim() || retentionError(s.retentionDays) != null);
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
        sign_in_by: s.auth === "oauth_user" && s.signInBy !== "key" ? SIGN_IN_BY[s.signInBy] : undefined,
        sign_in_retention_days: s.auth === "oauth_user" ? Number(s.retentionDays.trim()) : undefined,
      } satisfies OAuthConfig,
    }),
  };
}

/** Asks the admin API about the upstream's authorization server. */
function discover(kind: "mcp" | "a2a", url: string, client: "none" | "free" | "register") {
  return api<OAuthDiscovered>("/oauth/discover", {
    method: "POST",
    json: { kind, url: url.trim(), metadata_document: client !== "none", register: client === "register" },
  });
}

/** `s` with what discovery found filled in, keeping what was typed. */
function withFound(s: AuthState, d: OAuthDiscovered): AuthState {
  return {
    ...s,
    tokenUrl: d.token_url ?? s.tokenUrl,
    authorizationUrl: d.authorization_url ?? s.authorizationUrl,
    scopes: s.scopes || d.scopes.join(" "),
    resource: s.resource || d.resource || "",
    ...(d.client_id && { clientId: d.client_id, secret: d.client_secret ?? "", redirectUri: d.redirect_uri ?? "", tokenAuth: d.client_secret ? "" : "none" }),
    registerOnSave: getsClient(s) && !d.client_id && !!d.registration_url,
  };
}

/**
 * The auth to save: when the form waits to register the gateway at the authorization server, it
 * registers now. Call it on save, so typing a URL never registers anything.
 */
export async function withClient(kind: "mcp" | "a2a", url: string, s: AuthState): Promise<AuthState> {
  if (!(getsClient(s) && s.registerOnSave)) return s;
  const found = withFound(s, await discover(kind, url, "register"));
  if (!found.clientId.trim()) throw new Error("The authorization server registered no client; enter one registered there.");
  return { ...found, registerOnSave: false };
}

/** What discovery means for the client, in a sentence. */
function clientNote(d: OAuthDiscovered, s: AuthState): string {
  if (d.client === "metadata_document") return "The gateway signs in with its client metadata document (CIMD): nothing is registered there.";
  if (d.client === "registered") return "The gateway registered itself there.";
  if (!getsClient(s)) return "";
  if (d.registration_url) {
    return d.client_metadata_documents
      ? `Its client metadata documents (CIMD) need a public HTTPS address, so from ${location.origin} the gateway registers itself there when you save.`
      : "The gateway registers itself there when you save.";
  }
  return d.client_metadata_documents
    ? `It takes only client metadata documents (CIMD), which need a public HTTPS address, not ${location.origin}. Enter a client registered there.`
    : "It offers no registration: enter a client registered there.";
}

/**
 * The authentication fields of an MCP server's or A2A agent's form. `url` is what discovery starts
 * from: the MCP server's URL, or the agent's card. `urlField` renders that field, first, with the
 * Discover button to put at its end. With `auto` (a new MCP server), the form looks for the
 * server's OAuth as its URL is typed and picks it, until the authentication is chosen by hand.
 * `savedSignInPer`: what a saved upstream whose keys sign in keeps their sign-ins per, so the form
 * can warn that changing it removes them.
 */
export function AuthFields({
  kind,
  url,
  urlField,
  value: s,
  onChange,
  keepsSecret,
  auto = false,
  savedSignInPer,
}: {
  kind: "mcp" | "a2a";
  url: string;
  urlField: (discover: ReactNode) => ReactNode;
  value: AuthState;
  onChange: (s: AuthState) => void;
  keepsSecret: boolean;
  auto?: boolean;
  savedSignInPer?: SignInPer;
}) {
  const [found, setFound] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Discovery filled in the client: its fields fold away, as they need no changes.
  const [folded, setFolded] = useState<string | null>(null);
  // The authentication was chosen by hand: discovery no longer changes it.
  const [chosen, setChosen] = useState(false);
  const set = (changes: Partial<AuthState>) => onChange({ ...s, ...changes });
  const what = kind === "mcp" ? "server" : "agent";

  // The latest state for the auto-discovery effect, which runs on the URL only.
  const latest = useRef(s);
  latest.current = s;
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const autoFound = useRef(false);

  // Looks up the upstream's OAuth and fills it in. Without OAuth chosen yet, or while the URL is
  // typed (`quiet`), it picks each key signing in, afresh. The gateway's client there is its
  // metadata document where the server takes one; a registration waits for the save. Quietly, an
  // upstream without OAuth only undoes what an earlier URL picked.
  const run = async (target: string, quiet: boolean, live = () => true) => {
    setBusy(true);
    try {
      const cur = latest.current;
      const fresh = quiet || !isOAuth(cur.auth);
      const base: AuthState = fresh ? { ...authState(), auth: "oauth_user", header: cur.header } : cur;
      const d = await discover(kind, target, getsClient(base) ? "free" : "none");
      if (!live()) return;
      const next = withFound(base, d);
      autoFound.current = quiet;
      onChangeRef.current(next);
      setError(null);
      const lead = fresh ? `The ${what} signs in with OAuth: each key signs in to it on the Access page.` : "Found its authorization server.";
      setFound([lead, clientNote(d, base)].filter(Boolean).join(" "));
      const ready = next.auth === "oauth_user" && (next.clientId || next.registerOnSave);
      setFolded(ready ? `${host(d.authorization_url)} · ${d.client === "metadata_document" ? "client metadata document" : next.registerOnSave ? "registers on save" : "registered client"}` : null);
    } catch (e) {
      if (!live()) return;
      if (!quiet) {
        setFound(null);
        setError((e as Error).message);
      } else if (autoFound.current) {
        autoFound.current = false;
        onChangeRef.current(authState());
        setFound(null);
        setFolded(null);
      }
    } finally {
      if (live()) setBusy(false);
    }
  };
  const runRef = useRef(run);
  runRef.current = run;

  useEffect(() => {
    if (!auto || chosen) return;
    const target = url.trim();
    if (!/^https?:\/\/[^/\s]+\.[^/\s]/.test(target)) return;
    let live = true;
    const timer = setTimeout(() => runRef.current(target, true, () => live), 600);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [auto, chosen, url]);

  const oauth = isOAuth(s.auth);
  const discoverButton = (
    <Button
      variant="ghost"
      onClick={(e) => {
        // Inside the URL field's label: keep the click from focusing the input.
        e.preventDefault();
        run(url.trim(), false);
      }}
      disabled={busy || !url.trim()}
      title={`Find the ${what}'s OAuth sign-in from its ${kind === "mcp" ? "URL" : "card"}`}
    >
      {busy ? "Looking…" : "Discover"}
    </Button>
  );
  const fields = (
    <>
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
      <TextInput
        caption="Client ID"
        hint={
          s.auth === "oauth_user"
            ? "At authorization servers that take client metadata documents (CIMD), the URL of the gateway's: its address followed by /admin/auth/oauth/client.json."
            : undefined
        }
        mono
        value={s.clientId}
        onChange={(e) => set({ clientId: e.target.value, registerOnSave: false })}
        placeholder={s.registerOnSave ? "registered on save" : ""}
        required={!s.registerOnSave}
      />
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
    </>
  );
  return (
    <>
      {urlField(discoverButton)}
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
            <li>{auto ? "The form finds out when the server signs in with OAuth as you type its URL, and picks it. " : ""}Discover, in the {kind === "mcp" ? "URL" : "agent card"} field, looks it up and fills it in.</li>
          </ul>
        }
        value={s.auth}
        onChange={(a) => {
          setChosen(true);
          setFolded(null);
          set({ auth: a as McpAuth });
        }}
        options={(Object.keys(AUTH_LABEL) as McpAuth[]).map((value) => ({ value, label: AUTH_LABEL[value] }))}
      />
      {s.auth === "oauth_user" && (
        <Segmented
          caption="Sign in per"
          hint={
            <ul className="hint-list">
              <li>
                <strong>Key</strong>: each virtual key signs in once, on the Access page.
              </li>
              <li>
                <strong>Tenant</strong> or <strong>Tenant and user</strong>: for an application that serves many tenants with one key. Each tenant, or each user of a tenant, signs in
                on their own, told apart by the request's labels <code>tenant</code> and <code>user</code> (<code>x-gateway-labels: tenant=…,user=…</code>). A request without
                them is refused.
              </li>
              <li>
                Those who have not signed in get a sign-in link: in the error, as a sign-in tool in chat requests, or from <code>POST /v1/oauth/sign-ins</code>.
              </li>
              <li>
                The gateway trusts the labels: whoever holds the key can act as any tenant or user. Keep such a key on the application's server, which sets the labels from its
                own sign-in.
              </li>
            </ul>
          }
          value={s.signInBy}
          onChange={(v) => set({ signInBy: v })}
          options={(Object.keys(SIGN_IN_PER_LABEL) as SignInPer[]).map((value) => ({ value, label: SIGN_IN_PER_LABEL[value] }))}
        />
      )}
      {s.auth === "oauth_user" && (
        <TextInput
          caption="Keep unused sign-ins"
          hint={`Days a sign-in is kept without being used, ${RETENTION.min} to ${RETENTION.max}. Then it is deleted, and whoever signed in is asked to sign in again on their next request.`}
          inputMode="numeric"
          value={s.retentionDays}
          onChange={(e) => set({ retentionDays: e.target.value })}
          placeholder="days"
          note={retentionError(s.retentionDays)}
        />
      )}
      {s.auth === "oauth_user" && savedSignInPer && s.signInBy !== savedSignInPer && (
        <Banner tone="warning">Saving signs everyone out of the {what}: the sign-ins kept so far are per {SIGN_IN_PER_LABEL[savedSignInPer].toLowerCase()}.</Banner>
      )}
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
      {oauth && folded && (
        <Section title="OAuth client" summary={folded}>
          {fields}
        </Section>
      )}
      {oauth && !folded && (
        <fieldset>
          <legend>OAuth client</legend>
          {fields}
        </fieldset>
      )}
    </>
  );
}

/** The host of an address, for a summary. */
function host(url?: string | null): string {
  try {
    return url ? new URL(url).host : "";
  } catch {
    return "";
  }
}
