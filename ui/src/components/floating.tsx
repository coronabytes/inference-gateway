import { useLayoutEffect, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";

/**
 * A popup on its own layer: rendered at the end of the page and placed against the window, so a
 * dialog's scrolling body, a table or a card cannot clip it, and opening it never makes the page
 * around it grow. Give the popup the `floating` class and place it with `useFloating`.
 */
export function Floating({ children }: { children: ReactNode }) {
  return createPortal(children, document.body);
}

/**
 * While `open`, keeps `popup` under `anchor`, or above it when there is more room there; at least
 * as wide as the anchor (exactly as wide with `match`), and no taller than the room it has
 * (`.floating` in styles.css). Scrolling, resizing and the popup's own size changes move it along.
 */
export function useFloating(
  open: boolean,
  anchor: RefObject<HTMLElement | null>,
  popup: RefObject<HTMLElement | null>,
  match = false,
) {
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const p = popup.current;
      const a = anchor.current?.getBoundingClientRect();
      if (!p || !a) return;
      const { clientWidth: vw, clientHeight: vh } = document.documentElement;
      const gap = 6;
      const edge = 8;
      p.style.minWidth = `${a.width}px`;
      if (match) p.style.width = `${a.width}px`;
      const below = vh - a.bottom - gap - edge;
      const above = a.top - gap - edge;
      // The room it has, as `--room`, which the stylesheet caps its height with.
      p.style.removeProperty("--room");
      const up = p.offsetHeight > below && above > below;
      p.style.setProperty("--room", `${Math.max(0, up ? above : below)}px`);
      p.style.top = up ? "" : `${a.bottom + gap}px`;
      p.style.bottom = up ? `${vh - a.top + gap}px` : "";
      p.style.left = `${Math.max(edge, Math.min(a.left, vw - p.offsetWidth - edge))}px`;
    };
    place();
    const resized = new ResizeObserver(place);
    if (popup.current) resized.observe(popup.current);
    window.addEventListener("scroll", place, true);
    window.addEventListener("resize", place);
    return () => {
      resized.disconnect();
      window.removeEventListener("scroll", place, true);
      window.removeEventListener("resize", place);
    };
  }, [open, match]);
}
