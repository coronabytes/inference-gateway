import { useEffect, useState } from "react";

export type Theme = "light" | "dark";

const KEY = "gateway-theme";

/** The theme picked in the top bar, or the system's when there is none. */
export function initialTheme(): Theme {
  try {
    const saved = localStorage.getItem(KEY);
    if (saved === "light" || saved === "dark") return saved;
  } catch {
    // Storage can be blocked; follow the system then.
  }
  return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

/** Set the theme on <html>, where the stylesheet picks its palette, and on the browser chrome. */
export function applyTheme(theme: Theme) {
  document.documentElement.dataset.theme = theme;
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", theme === "light" ? "#f3f5fb" : "#03050b");
}

const WIDE_KEY = "gateway-wide";

/**
 * Widescreen: pages use the whole window instead of a centred column. Remembered in this browser
 * and set on <html> as data-wide, where the stylesheet lifts the width limit.
 */
export function useWide(): [boolean, () => void] {
  const [wide, setWide] = useState(() => {
    try {
      return localStorage.getItem(WIDE_KEY) === "1";
    } catch {
      return false;
    }
  });
  useEffect(() => {
    if (wide) document.documentElement.dataset.wide = "";
    else delete document.documentElement.dataset.wide;
  }, [wide]);
  const toggle = () => {
    try {
      localStorage.setItem(WIDE_KEY, wide ? "0" : "1");
    } catch {
      // Not remembered, but still switched for this visit.
    }
    setWide(!wide);
  };
  return [wide, toggle];
}

export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState(initialTheme);
  useEffect(() => applyTheme(theme), [theme]);
  const toggle = () => {
    const next = theme === "light" ? "dark" : "light";
    try {
      localStorage.setItem(KEY, next);
    } catch {
      // Not remembered, but still switched for this visit.
    }
    setTheme(next);
  };
  return [theme, toggle];
}
