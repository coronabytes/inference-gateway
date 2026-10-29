import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type { BudgetPeriod, BudgetStatus } from "../api";
import { dateTime, until, usd } from "../format";

const PERIOD_LABEL: Record<BudgetPeriod, string> = { day: "per day", week: "per week", month: "per month" };

/** A key's or project's spend this period against its budget, and when the period resets. */
export function BudgetMeter({ budget: b }: { budget: BudgetStatus }) {
  const frac = Math.min(1, b.limit_usd > 0 ? b.spent_usd / b.limit_usd : 1);
  return (
    <div className="spend">
      <span>
        {usd(b.spent_usd)} <span className="muted">/ {usd(b.limit_usd)} {PERIOD_LABEL[b.period]}</span>
      </span>
      <div
        className={`meter ${frac >= 1 ? "meter-critical" : frac >= 0.8 ? "meter-warning" : ""}`}
        role="meter"
        aria-valuenow={Math.round(frac * 100)}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label="Budget used this period"
      >
        <div style={{ width: `${frac * 100}%` }} />
      </div>
      <span className="muted small" title={dateTime(b.resets_at)}>
        resets {until(b.resets_at)}
      </span>
    </div>
  );
}

export function StatTile({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="tile">
      <div className="tile-label">{label}</div>
      <div className="tile-value">{value}</div>
      {hint && <div className="tile-hint">{hint}</div>}
    </div>
  );
}

/**
 * A dialog no taller than the window, at the end of the page so no panel around it can trap it. The title with the close button and the `footer` (the
 * dialog's buttons) stay in view; the content between them scrolls, with a line above and below
 * it while it does.
 */
/** Open dialogs, innermost last: Escape closes only the innermost one. */
const openDialogs: object[] = [];

export function Modal({
  title,
  onClose,
  children,
  footer,
  size,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  /** Wider than the default 520px: `medium` 600px, for single URLs on a line; `wide` 820px. */
  size?: "medium" | "wide";
}) {
  const body = useRef<HTMLDivElement>(null);
  const [scrolls, setScrolls] = useState(false);
  // The latest onClose, so a parent that passes a new function on every render keeps its place.
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const me = {};
    openDialogs.push(me);
    const onKey = (e: globalThis.KeyboardEvent) => e.key === "Escape" && openDialogs[openDialogs.length - 1] === me && close.current();
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      openDialogs.splice(openDialogs.indexOf(me), 1);
    };
  }, []);
  // After every render, as the content may have grown or shrunk, and when the window resizes.
  const measure = () => {
    const b = body.current;
    if (b) setScrolls(b.scrollHeight > b.clientHeight + 1);
  };
  useLayoutEffect(measure);
  useEffect(() => {
    const resized = new ResizeObserver(measure);
    if (body.current) resized.observe(body.current);
    return () => resized.disconnect();
  }, []);
  return createPortal(
    <div className="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className={`modal${size ? ` modal-${size}` : ""}${scrolls ? " scrolls" : ""}`} role="dialog" aria-modal="true" aria-label={title}>
        <div className="modal-head">
          <h2>{title}</h2>
          <button className="ghost icon" onClick={onClose} aria-label="Close">
            ×
          </button>
        </div>
        <div className="modal-body" ref={body}>
          {children}
        </div>
        {footer && <div className="modal-foot">{footer}</div>}
      </div>
    </div>,
    document.body,
  );
}

/** A pending confirmation: what to ask and what to run if the user agrees. */
export interface ConfirmRequest {
  title: string;
  message: ReactNode;
  confirmLabel?: string;
  action: () => Promise<unknown>;
}

/**
 * In-app confirmation dialog. Used instead of `window.confirm`, which embedded browsers may block
 * (the click then silently does nothing). Errors from the action are shown in the dialog.
 */
export function ConfirmDialog({ request, onClose }: { request: ConfirmRequest; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async () => {
    setBusy(true);
    try {
      await request.action();
      onClose();
    } catch (e) {
      setError((e as Error).message);
      setBusy(false);
    }
  };
  return (
    <Modal
      title={request.title}
      onClose={onClose}
      footer={
        <>
          <button onClick={onClose} disabled={busy} autoFocus>Cancel</button>
          <button className="danger-solid" onClick={run} disabled={busy}>
            {busy ? "Working…" : (request.confirmLabel ?? "Delete")}
          </button>
        </>
      }
    >
      <ErrorBanner error={error} />
      <p className="confirm-message">{request.message}</p>
    </Modal>
  );
}

export function IconButton({ label, onClick, danger, disabled, title, children }: { label: string; onClick: () => void; danger?: boolean; disabled?: boolean; title?: string; children: ReactNode }) {
  return (
    <button className={`icon-btn${danger ? " danger" : ""}`} onClick={onClick} disabled={disabled} aria-label={label} title={title ?? label}>
      {children}
    </button>
  );
}

/** Edit: a vector pen nib, more technical than a pencil. */
export function EditIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M15.7 21.3a1 1 0 0 1-1.4 0l-1.6-1.6a1 1 0 0 1 0-1.4l5.6-5.6a1 1 0 0 1 1.4 0l1.6 1.6a1 1 0 0 1 0 1.4Z" />
      <path d="m18 13-1.4-6.9a1 1 0 0 0-.7-.8L3.2 2a1 1 0 0 0-1.2 1.2l3.4 12.6a1 1 0 0 0 .8.7L13 18" />
      <path d="m2.3 2.3 7.3 7.3" />
      <circle cx="11" cy="11" r="2" />
    </svg>
  );
}

/** Try something out on a sample: a lab flask. */
export function TryIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M9 3h6" />
      <path d="M10 3v6.5L4.6 18.4A1.7 1.7 0 0 0 6 21h12a1.7 1.7 0 0 0 1.4-2.6L14 9.5V3" />
      <path d="M7.5 15h9" />
    </svg>
  );
}

/** Attach files: a paper clip. */
export function AttachIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="m21.4 11.1-9.2 9.2a6 6 0 0 1-8.5-8.5l8.6-8.6a4 4 0 0 1 5.7 5.7l-8.6 8.6a2 2 0 0 1-2.8-2.8l8.5-8.5" />
    </svg>
  );
}

/** A menu of more actions: three bars. */
export function MenuIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" aria-hidden="true">
      <path d="M4 6h16M4 12h16M4 18h16" />
    </svg>
  );
}

/** Down and to the right: a row that belongs to the one above it. */
export function ChildArrowIcon() {
  return (
    <svg className="child-arrow" width="26" height="14" viewBox="0 0 26 14" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M2 1v5a4 4 0 0 0 4 4h18M20 6l4 4-4 4" />
    </svg>
  );
}

export function TrashIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M3 6h18" />
      <path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
      <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
      <path d="M10 11v6M14 11v6" />
    </svg>
  );
}

/** Disable (pause) a key. */
export function PauseIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="10" />
      <path d="M10 15V9M14 15V9" />
    </svg>
  );
}

/** Enable (resume) a key. */
export function PlayIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="10" />
      <path d="M10 8l6 4-6 4Z" />
    </svg>
  );
}

/** Reset (start over). */
export function ResetIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M3 12a9 9 0 1 0 3-6.7L3 8" />
      <path d="M3 3v5h5" />
    </svg>
  );
}

/** Switch to the light theme. */
export function SunIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41" />
    </svg>
  );
}

/** Switch to the dark theme. */
/** An open book: the API reference. */
export function DocsIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M2 4h6a4 4 0 0 1 4 4v13a3 3 0 0 0-3-3H2z" />
      <path d="M22 4h-6a4 4 0 0 0-4 4v13a3 3 0 0 1 3-3h7z" />
    </svg>
  );
}

/** Widescreen on: arrows pointing out to the window's edges. */
export function WidenIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M3 5v14M21 5v14M7 12h10M10 9l-3 3 3 3M14 9l3 3-3 3" />
    </svg>
  );
}

/** Widescreen off: arrows pointing in to a centred column. */
export function NarrowIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M12 5v14M3 12h6M21 12h-6M6 9l3 3-3 3M18 9l-3 3 3 3" />
    </svg>
  );
}

export function MoonIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8Z" />
    </svg>
  );
}

/**
 * A form section that starts collapsed. `summary` sums up its values beside the title, so a closed
 * section still shows what it holds; the fields stay mounted while closed.
 */
export function Section({ title, summary, children }: { title: string; summary?: string; children: ReactNode }) {
  return (
    <details className="section">
      <summary>
        <span>{title}</span>
        {summary && <span className="section-summary">{summary}</span>}
      </summary>
      <div className="section-body">{children}</div>
    </details>
  );
}

export function Badge({ tone, children }: { tone: "good" | "warning" | "critical" | "neutral"; children: ReactNode }) {
  const icon = { good: "●", warning: "▲", critical: "■", neutral: "○" }[tone];
  return (
    <span className={`badge badge-${tone}`}>
      <span aria-hidden="true">{icon}</span>
      {children}
    </span>
  );
}

export function ErrorBanner({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <div className="banner" role="alert">
      <Badge tone="critical">Error</Badge> {error}
    </div>
  );
}

/** Parse an optional positive integer from an input string. */
export function optInt(s: string): number | null {
  const n = parseInt(s, 10);
  return Number.isFinite(n) && n > 0 ? n : null;
}

export function optNum(s: string): number | null {
  if (s.trim() === "") return null;
  const n = Number(s);
  return Number.isFinite(n) && n >= 0 ? n : null;
}
