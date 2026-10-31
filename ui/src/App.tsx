import { useCallback, useEffect, useState, type FormEvent, type ReactNode } from "react";
import { SessionContext } from "./access";
import { AuthError, api, setToken, type SessionInfo } from "./api";
import { Banner } from "./components/controls";
import { Galaxy } from "./components/Galaxy";
import { DocsIcon, IconButton, MoonIcon, SunIcon } from "./components/ui";
import { UserMenu } from "./components/UserMenu";
import { Cluster } from "./pages/Cluster";
import { Connect } from "./pages/Connect";
import { Keys } from "./pages/Keys";
import { Logs, RequestLog } from "./pages/Logs";
import { Metrics } from "./pages/Metrics";
import { Models } from "./pages/Models";
import { Playground } from "./pages/Playground";
import { Tools } from "./pages/Tools";
import { useTheme } from "./theme";

type PageId = "metrics" | "logs" | "access" | "models" | "tools" | "cluster";

const PAGES: Record<PageId, { label: string; el: ReactNode }> = {
  metrics: { label: "Metrics", el: <Metrics /> },
  models: { label: "Models", el: <Models /> },
  tools: { label: "Tools", el: <Tools /> },
  access: { label: "Access", el: <Keys /> },
  logs: { label: "Logs", el: <Logs /> },
  cluster: { label: "Cluster", el: <Cluster /> },
};

/** A user's console: their projects' keys and metrics, the playground and the request log. The
 * pages keep their addresses, so links work for admins and users alike. */
const USER_PAGES: Partial<Record<PageId, { label: string; el: ReactNode }>> = {
  metrics: PAGES.metrics,
  models: {
    label: "Playground",
    el: (
      <div className="page">
        <Playground />
      </div>
    ),
  },
  access: { label: "Keys", el: <Keys /> },
  logs: {
    label: "Request log",
    el: (
      <div className="page">
        <RequestLog />
      </div>
    ),
  },
};

/** Former page addresses (pages since renamed or made tabs of another); old links and bookmarks still open them. */
const MOVED: Record<string, string> = {
  credentials: "models/credentials",
  playground: "models/playground",
  "cluster/requests": "logs",
  "cluster/audit": "logs/audit",
  settings: "cluster/settings",
  "cluster/admins": "access/users",
  keys: "access",
  "keys/projects": "access",
  "access/projects": "access",
};

/** The page in the address (`#/page` or `#/page/tab`). */
function currentPage(): PageId {
  // A page may take parameters after it: `#/page?name=value`.
  const h = window.location.hash.replace(/^#\/?/, "").split("?")[0];
  if (h in MOVED) window.history.replaceState(null, "", `#/${MOVED[h]}`);
  const page = (MOVED[h] ?? h).split("/")[0];
  return (page in PAGES ? page : "metrics") as PageId;
}

/** An MCP client's sign-in waiting for approval: `#/connect/{id}`. */
function connectId(): string | null {
  return window.location.hash.match(/^#\/connect\/([0-9a-f]+)$/)?.[1] ?? null;
}

/** sessionStorage: the sign-in to come back to once signed in, as single sign-on returns to `/`. */
const CONNECT_RETURN = "gw.connect";

/** A sign-in error the gateway sent back from the identity provider, once. */
function takeLoginError(): string | null {
  const params = new URLSearchParams(window.location.search);
  const error = params.get("login_error");
  if (error) window.history.replaceState(null, "", window.location.pathname + window.location.hash);
  return error;
}

export function App() {
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [page, setPage] = useState<PageId>(currentPage);
  const [connect, setConnect] = useState(connectId);
  const [loginError] = useState(takeLoginError);
  const [theme, toggleTheme] = useTheme();

  const refresh = useCallback(async () => {
    try {
      setSession(await api<SessionInfo>("/auth/session"));
    } catch {
      // A stale master key makes the request fail; drop it and ask again.
      setToken(null);
      setSession(await api<SessionInfo>("/auth/session").catch(() => null));
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  useEffect(() => {
    const onHash = () => {
      setPage(currentPage());
      setConnect(connectId());
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  // Any 401 from a page signs out.
  useEffect(() => {
    const onUnauthorized = () => {
      setToken(null);
      refresh();
    };
    window.addEventListener("gw-unauthorized", onUnauthorized);
    return () => window.removeEventListener("gw-unauthorized", onUnauthorized);
  }, [refresh]);

  const signOut = async () => {
    if (session?.via === "session" || session?.via === "dev" || session?.via === "dev_user") {
      await api("/auth/logout", { method: "POST" }).catch(() => undefined);
    }
    setToken(null);
    await refresh();
  };

  // Dev Login only. A full reload, so every page loads again with what the new role may see.
  const switchDev = async (role: "admin" | "user") => {
    await api(`/auth/dev-login?role=${role}`, { method: "POST" });
    window.location.reload();
  };

  if (session === null) return null;
  if (!session.authenticated) {
    if (connect) {
      try {
        sessionStorage.setItem(CONNECT_RETURN, connect);
      } catch {
        // Without storage, a sign-in that leaves the page lands on the metrics; the app can start again.
      }
    }
    return <Login sso={session.sso} masterKey={session.master_key} dev={session.dev} version={session.version} initialError={loginError} onLogin={refresh} />;
  }

  // Back to an MCP client's sign-in that had to wait for the console's own.
  if (!connect) {
    let back: string | null = null;
    try {
      back = sessionStorage.getItem(CONNECT_RETURN);
      sessionStorage.removeItem(CONNECT_RETURN);
    } catch {
      // Nothing to come back to.
    }
    if (back) window.location.hash = `#/connect/${back}`;
  }

  const user = session.role === "user";
  const pages = user ? USER_PAGES : PAGES;
  // A user can sign in before an admin adds them to a project; until then there is nothing to show.
  const idle = user && !session.projects?.length;

  return (
    <SessionContext.Provider value={session}>
    <div className="shell">
      {theme === "light" ? <div className="app-sky" aria-hidden="true" /> : <Galaxy className="app-sky" backdrop />}
      <header className="topbar">
        <div className="brand">
          Inference Gateway
          <span className="version-pill" title="Gateway version">
            {session.version}
          </span>
        </div>
        <nav>
          {!idle &&
            (Object.entries(pages) as [PageId, { label: string }][]).map(([id, { label }]) => (
              <a key={id} href={`#/${id}`} className={page === id && !connect ? "active" : ""} aria-current={page === id && !connect ? "page" : undefined} data-label={label}>
                {label}
              </a>
            ))}
        </nav>
        <div className="who">
          {session.api_docs && (
            <a className="icon-btn" href="/admin/docs/" target="_blank" rel="noopener" aria-label="API reference" title="API reference">
              <DocsIcon />
            </a>
          )}
          <IconButton label={theme === "light" ? "Switch to dark theme" : "Switch to light theme"} onClick={toggleTheme}>
            {theme === "light" ? <MoonIcon /> : <SunIcon />}
          </IconButton>
          <UserMenu session={session} onSignOut={signOut} onSwitch={switchDev} />
        </div>
      </header>
      <main>
        {connect ? (
          <Connect id={connect} />
        ) : idle ? (
          <div className="empty-card">
            You are signed in, but not part of any project yet. Once an admin adds you to one, you manage its keys here and see its
            playground, request log and metrics.
          </div>
        ) : (
          // A user who follows an admin's link to a page they do not have sees the metrics.
          (pages[page] ?? PAGES.metrics).el
        )}
      </main>
    </div>
    </SessionContext.Provider>
  );
}

function Login({
  sso,
  masterKey,
  dev,
  version,
  initialError,
  onLogin,
}: {
  sso: SessionInfo["sso"];
  masterKey: boolean;
  dev: boolean;
  version: string;
  initialError: string | null;
  onLogin: () => void;
}) {
  const [key, setKey] = useState("");
  const [error, setError] = useState<string | null>(initialError);
  const [showKey, setShowKey] = useState(!sso && !dev);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setToken(key.trim());
    try {
      const s = await api<SessionInfo>("/auth/session");
      if (!s.authenticated) throw new AuthError("rejected");
      onLogin();
    } catch (err) {
      setToken(null);
      setError(err instanceof AuthError ? "That master key was rejected." : (err as Error).message);
    }
  };

  // As an admin, or as the local development user, whom admins add to projects like anyone else.
  const devLogin = async (role: "admin" | "user") => {
    try {
      await api(`/auth/dev-login?role=${role}`, { method: "POST" });
      onLogin();
    } catch (err) {
      setError((err as Error).message);
    }
  };

  return (
    <div className="login">
      <Galaxy className="login-sky" />
      <div className="login-panel">
        <p className="login-eyebrow">
          Admin console
          <span className="version-pill" title="Gateway version">
            {version}
          </span>
        </p>
        <h1 className="login-title">Inference Gateway</h1>
        <form onSubmit={submit} className="login-card">
          {error && <Banner onDismiss={() => setError(null)}>{error}</Banner>}
          {dev && (
            <>
              <button type="button" className="login-primary" onClick={() => devLogin("admin")}>
                Dev Login as admin
              </button>
              <button type="button" className="login-secondary" onClick={() => devLogin("user")}>
                Dev Login as user
              </button>
            </>
          )}
          {sso && (
            <a className={`button ${dev ? "login-secondary" : "login-primary"}`} href="/admin/auth/oidc/login">
              {sso.display_name}
            </a>
          )}
          {!sso && !masterKey && !dev && (
            <p className="login-note">
              Sign-in is not set up. Set <code>GATEWAY_MASTER_KEY</code> and restart the gateway, then sign in with it to configure single sign-on.
            </p>
          )}
          {!masterKey ? null : showKey ? (
            <>
              <p className="login-note">
                {sso || dev ? "Or use the gateway master key." : "Sign in with the gateway master key."} It is kept only for this browser tab.
              </p>
              <input
                type="password"
                autoComplete="current-password"
                value={key}
                onChange={(e) => setKey(e.target.value)}
                placeholder="GATEWAY_MASTER_KEY"
                autoFocus={!sso && !dev}
                aria-label="Master key"
              />
              <button className={sso || dev ? "login-secondary" : "login-primary"} disabled={!key.trim()}>
                Sign in with master key
              </button>
            </>
          ) : (
            <button type="button" className="login-link" onClick={() => setShowKey(true)}>
              Use the master key instead
            </button>
          )}
        </form>
      </div>
    </div>
  );
}
