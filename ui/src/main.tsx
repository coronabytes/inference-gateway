import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./styles.css";
import { applyTheme, initialTheme } from "./theme";

// Before the first render, so the page does not flash in the other theme.
applyTheme(initialTheme());

// The window's width without its scrollbar, which CSS cannot measure (100vw includes it): the
// pinned band under the top bar spans exactly this, so its blur never reaches past the edge.
const root = document.documentElement;
new ResizeObserver(() => root.style.setProperty("--page-width", `${root.clientWidth}px`)).observe(root);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
