import { useEffect, useId, useRef, useState } from "react";
import type { SessionInfo } from "../api";

const VIA: Record<NonNullable<SessionInfo["via"]>, string> = {
  session: "Signed in with single sign-on",
  master_key: "Signed in with the master key",
  virtual_key: "Signed in with a virtual key",
  project_key: "Signed in with a project admin key",
  dev: "Signed in with dev login",
  dev_user: "Signed in with dev login as a user",
};

/**
 * The profile button at the end of the top bar. Who is signed in, and how, shows in its popover
 * along with Sign out. Closes on a click outside or Escape.
 */
export function UserMenu({
  session,
  onSignOut,
  onSwitch,
}: {
  session: SessionInfo;
  onSignOut: () => void;
  /** Dev Login only: sign in again as the admin or as the local development user. */
  onSwitch: (role: "admin" | "user") => void;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const popId = useId();

  const name = session.user?.name || session.user?.email || (session.via === "master_key" ? "Master key" : "Signed in");
  // The email under the name, unless it already is the name.
  const email = session.user?.email && session.user.email !== name ? session.user.email : null;

  useEffect(() => {
    if (!open) return;
    const outside = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    const escape = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setOpen(false);
      button.current?.focus();
    };
    document.addEventListener("mousedown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);

  return (
    <div className="user-menu" ref={root}>
      <button
        ref={button}
        className={`icon-btn${open ? " open" : ""}`}
        onClick={() => setOpen((o) => !o)}
        aria-label={`Account: ${name}`}
        title={name}
        aria-expanded={open}
        aria-controls={popId}
      >
        <UserIcon />
      </button>
      {open && (
        <div className="user-pop" id={popId}>
          <div className="user-pop-who">
            <strong>{name}</strong>
            {email && <span className="muted small">{email}</span>}
            {session.via && <span className="muted small">{VIA[session.via]}</span>}
            {session.role === "user" && <span className="muted small">User: sees only their projects</span>}
          </div>
          {(session.via === "dev" || session.via === "dev_user") && (
            <button className="ghost user-pop-action" onClick={() => onSwitch(session.via === "dev" ? "user" : "admin")}>
              {session.via === "dev" ? "Switch to user" : "Switch to admin"}
            </button>
          )}
          <button className="ghost user-pop-action" onClick={onSignOut}>
            Sign out
          </button>
        </div>
      )}
    </div>
  );
}

function UserIcon() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="10" />
      <circle cx="12" cy="10" r="3" />
      <path d="M6.17 18.65a7 7 0 0 1 11.66 0" />
    </svg>
  );
}
