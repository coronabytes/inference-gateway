import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type { BudgetPeriod, BudgetStatus } from "../api";
import { compact, dateTime, until, usd } from "../format";
import { Banner } from "./controls";

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

/**
 * A panel along the right edge, over the page without a backdrop: the page keeps its size and
 * stays usable, and picking another item swaps the panel's content. × or Escape closes it, unless
 * a dialog is open over it. Its body scrolls under a fixed title.
 */
export function SidePanel({ title, onClose, children }: { title: ReactNode; onClose: () => void; children: ReactNode }) {
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => e.key === "Escape" && openDialogs.length === 0 && close.current();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  return createPortal(
    <aside className="side-panel" aria-label={typeof title === "string" ? title : "Details"}>
      <div className="side-panel-head">
        <h2>{title}</h2>
        <button className="ghost icon" onClick={onClose} aria-label="Close">
          ×
        </button>
      </div>
      <div className="side-panel-body">{children}</div>
    </aside>,
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
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
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

/** Image input: a picture. */
export function ImageIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <rect x="3" y="3" width="18" height="18" rx="2" />
      <circle cx="9" cy="9" r="2" />
      <path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21" />
    </svg>
  );
}

/** Document (PDF) input: a page with a folded corner. */
export function DocumentIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z" />
      <path d="M14 3v6h6M8 13h8M8 17h5" />
    </svg>
  );
}

/** Reasoning: a light bulb. */
export function ReasoningIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M9 18h6M10 22h4" />
      <path d="M12 2a7 7 0 0 0-4 12.7c.6.5 1 1.3 1 2.3h6c0-1 .4-1.8 1-2.3A7 7 0 0 0 12 2z" />
    </svg>
  );
}

/** Structured (JSON) output: curly braces. */
export function BracesIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M8 3H7a2 2 0 0 0-2 2v5a2 2 0 0 1-2 2 2 2 0 0 1 2 2v5a2 2 0 0 0 2 2h1" />
      <path d="M16 3h1a2 2 0 0 1 2 2v5a2 2 0 0 0 2 2 2 2 0 0 0-2 2v5a2 2 0 0 1-2 2h-1" />
    </svg>
  );
}

/** Search the web: a globe. */
export function GlobeIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="9" />
      <path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18" />
    </svg>
  );
}

/** A chat model: a speech bubble. */
export function ChatIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
    </svg>
  );
}

/** An embedding model: a vector in its axes. */
export function EmbeddingIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M4 3v17h17" />
      <path d="m4 20 11-11M10 9h5v5" />
    </svg>
  );
}

/** A decision model: a path that forks. */
export function DecisionIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M16 3h5v5M8 3H3v5" />
      <path d="M12 22v-8.3a4 4 0 0 0-1.2-2.9L3 3M15 9l6-6" />
    </svg>
  );
}

/** A rerank model: arrows that reorder. */
export function RerankIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="m21 16-4 4-4-4M17 20V4" />
      <path d="m3 8 4-4 4 4M7 4v16" />
    </svg>
  );
}

/** A router: a route between two stops. */
export function RouterIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="6" cy="19" r="3" />
      <circle cx="18" cy="5" r="3" />
      <path d="M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15" />
    </svg>
  );
}

/** A model alias: a link to the model it serves. */
export function AliasIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7" />
      <path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7" />
    </svg>
  );
}

/** Reasoning effort: a gauge. */
export function EffortIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M4.5 18a9 9 0 1 1 15 0" />
      <path d="M12 13.5 16 9" />
      <circle cx="12" cy="14" r="1" />
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

/** Sign-ins to other services: a plug. */
export function PlugIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M12 22v-5" />
      <path d="M9 8V2" />
      <path d="M15 8V2" />
      <path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z" />
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

/** A view of cards: a 2×2 grid. */
export function TilesIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <rect x="3" y="3" width="7" height="7" rx="1.5" />
      <rect x="14" y="3" width="7" height="7" rx="1.5" />
      <rect x="3" y="14" width="7" height="7" rx="1.5" />
      <rect x="14" y="14" width="7" height="7" rx="1.5" />
    </svg>
  );
}

/** A view of rows: a bulleted list. */
export function ListIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M9 6h12M9 12h12M9 18h12M4 6h.01M4 12h.01M4 18h.01" />
    </svg>
  );
}

/** Details: an "i" in a circle. */
export function DetailsIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 16v-5M12 8h.01" />
    </svg>
  );
}

/** Copy to the clipboard: two overlapping sheets. */
export function CopyIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <rect x="9" y="9" width="12" height="12" rx="2" />
      <path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1" />
    </svg>
  );
}

/** Token prices per 1M, input above output (left out for embeddings), then the long-context threshold and its prices in a second column. */
export function TokenPrices({ input, output, long }: { input: ReactNode; output?: ReactNode; long?: { threshold: number; input: ReactNode; output?: ReactNode } }) {
  const out = output != null;
  return (
    <div className={`stat-pairs num${long ? " wide" : ""}`} title="$ per 1M tokens">
      <span>{input}</span>
      <span className="muted small">in</span>
      {long && (
        <span className="muted small threshold" title={`Above ${long.threshold.toLocaleString()} input tokens`}>
          &gt; {compact(long.threshold)}
        </span>
      )}
      {long && <span className="long">{long.input}</span>}
      {long && <span className="muted small">in</span>}
      {out && <span>{output}</span>}
      {out && <span className="muted small">out</span>}
      {out && long && <span className="long">{long.output}</span>}
      {out && long && <span className="muted small">out</span>}
    </div>
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

/** A failed request's error as a `Banner`, shown while `error` is set; Dismiss clears it. */
export function ErrorBanner({ error, onDismiss }: { error: string | null; onDismiss: () => void }) {
  if (!error) return null;
  return <Banner onDismiss={onDismiss}>{error}</Banner>;
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
