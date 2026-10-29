import { createContext, useContext, useEffect, useId, useRef, useState, type MouseEvent, type ReactNode } from "react";
import { Floating, useFloating } from "./floating";

/*
 * Hints for captioned controls (controls.tsx). A control's hint stays out of the way until its
 * caption is clicked; then it opens as a popover under the control. A click elsewhere or Escape
 * closes it. The hint is always the control's accessible description (`aria-describedby`), so
 * screen readers announce it without the click.
 */

interface Hint {
  /** The hint element's id, for the control's `aria-describedby`. */
  id: string;
  open: boolean;
  toggle: () => void;
}

const HintContext = createContext<Hint | null>(null);

/** The hint of the control being rendered, if it has one. */
export function useHint(): Hint | null {
  return useContext(HintContext);
}

/**
 * Wraps a control and its hint. Without a hint (and without a class) it renders the control alone.
 * `note` is shown below the control at all times, for messages that must not hide (errors).
 */
export function HintBox({
  hint,
  note,
  id: given,
  className,
  children,
}: {
  hint?: ReactNode;
  note?: ReactNode;
  /** The hint element's id, for controls that set their own `aria-describedby`. */
  id?: string;
  className?: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const generated = useId();
  const id = given ?? generated;
  const root = useRef<HTMLDivElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  const has = hint != null && hint !== false && hint !== "";
  useFloating(open, root, pop, true);

  useEffect(() => {
    if (!open) return;
    const away = (e: PointerEvent) => {
      const t = e.target as Node;
      if (!root.current?.contains(t) && !pop.current?.contains(t)) setOpen(false);
    };
    // Capture, so Escape closes the hint before a dialog around it sees the key.
    const key = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopImmediatePropagation();
      setOpen(false);
    };
    document.addEventListener("pointerdown", away);
    window.addEventListener("keydown", key, true);
    return () => {
      document.removeEventListener("pointerdown", away);
      window.removeEventListener("keydown", key, true);
    };
  }, [open]);

  if (!has && note == null && !className) return <>{children}</>;
  return (
    <div className={["field", has && "has-hint", className].filter(Boolean).join(" ")} ref={root}>
      <HintContext.Provider value={has ? { id, open, toggle: () => setOpen((o) => !o) } : null}>{children}</HintContext.Provider>
      {has && (
        <Floating>
          <div id={id} role="note" className="hint-pop floating" hidden={!open} ref={pop}>
            {hint}
          </div>
        </Floating>
      )}
      {note != null && <div className="field-hint">{note}</div>}
    </div>
  );
}

/**
 * The caption inside a control. When the control has a hint, clicking the caption opens it; the
 * click goes no further, so it neither focuses the input nor opens a dropdown around the caption.
 */
export function Caption({ children, htmlFor }: { children: ReactNode; htmlFor?: string }) {
  const hint = useHint();
  if (!hint) {
    return htmlFor ? (
      <label className="control-caption" htmlFor={htmlFor}>
        {children}
      </label>
    ) : (
      <span className="control-caption">{children}</span>
    );
  }
  const toggle = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    hint.toggle();
  };
  return (
    <span className={hint.open ? "control-caption has-hint open" : "control-caption has-hint"} onClick={toggle} title="Show the hint">
      {children}
    </span>
  );
}
