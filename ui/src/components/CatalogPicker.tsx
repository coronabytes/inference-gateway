import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { api, type PriceEntry, type PriceSearch } from "../api";
import { catalogPrices, perMillion } from "../pricing";
import { Floating, useFloating } from "./floating";
import { Caption, HintBox } from "./hint";

/**
 * Free-text input that searches the price catalog and offers matching entries to pick.
 * `providers` and `modes` (LiteLLM's names) narrow the suggestions; empty means all.
 */
export function CatalogPicker({
  value,
  onChange,
  onPick,
  providers = [],
  modes = [],
  placeholder,
  caption,
  hint,
  required,
  autoFocus,
}: {
  value: string;
  onChange: (v: string) => void;
  onPick: (entry: PriceEntry) => void;
  providers?: string[];
  modes?: string[];
  placeholder?: string;
  /** Shown inside the field before the value; also its accessible name. */
  caption: string;
  hint?: ReactNode;
  required?: boolean;
  autoFocus?: boolean;
}) {
  const hintId = useId();
  const [open, setOpen] = useState(false);
  const [results, setResults] = useState<PriceEntry[]>([]);
  const [active, setActive] = useState(0);
  // Leaving the field closes the list a moment later, so a click on a suggestion still lands.
  // Coming back first cancels that.
  const closing = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const show = () => {
    clearTimeout(closing.current);
    setOpen(true);
  };
  const scope = `${providers.join(",")}|${modes.join(",")}`;
  const listShown = open && results.length > 0;
  const field = useRef<HTMLLabelElement>(null);
  const list = useRef<HTMLUListElement>(null);
  useFloating(listShown, field, list, true);

  useEffect(() => {
    if (!open || !value.trim()) {
      setResults([]);
      return;
    }
    const t = setTimeout(async () => {
      const params = new URLSearchParams({ q: value, limit: "8", best: "true" });
      if (providers.length) params.set("provider", providers.join(","));
      if (modes.length) params.set("mode", modes.join(","));
      try {
        const r = await api<PriceSearch>(`/prices?${params}`);
        setResults(r.items);
        setActive(0);
      } catch {
        setResults([]);
      }
    }, 150);
    return () => clearTimeout(t);
    // `scope` stands in for the provider and mode lists, which are new arrays on every render.
  }, [value, open, scope]);

  const pick = (e: PriceEntry) => {
    onPick(e);
    setOpen(false);
  };

  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape") setOpen(false);
    if (!open || !results.length) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((a) => (a + 1) % results.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => (a - 1 + results.length) % results.length);
    } else if (e.key === "Enter") {
      e.preventDefault();
      pick(results[active]);
    }
  };

  const picker = (
    <div className="combo">
      <label className="control" ref={field}>
        <Caption>{caption}</Caption>
        <input
          className="mono"
          value={value}
          onChange={(e) => {
            onChange(e.target.value);
            show();
          }}
          onFocus={show}
          onBlur={() => {
            closing.current = setTimeout(() => setOpen(false), 120);
          }}
          onKeyDown={onKey}
          placeholder={placeholder}
          required={required}
          autoFocus={autoFocus}
          role="combobox"
          aria-expanded={listShown}
          aria-autocomplete="list"
          aria-describedby={hint ? hintId : undefined}
        />
      </label>
      {listShown && (
        <Floating>
          <ul className="combo-list floating" ref={list} role="listbox" aria-label={`${caption} suggestions from the price catalog`}>
            {results.map((r, i) => {
              const p = catalogPrices(r.data);
              return (
                <li
                  key={r.key}
                  role="option"
                  aria-selected={i === active}
                  className={i === active ? "active" : undefined}
                  onMouseEnter={() => setActive(i)}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    pick(r);
                  }}
                >
                  <span className="mono">{r.key}</span>
                  <span className="muted small">
                    {[p.provider, p.mode].filter(Boolean).join(" · ")} · in {perMillion(p.input)}
                    {p.output ? ` / out ${perMillion(p.output)}` : ""}
                  </span>
                </li>
              );
            })}
          </ul>
        </Floating>
      )}
    </div>
  );
  return (
    <HintBox hint={hint} id={hintId}>
      {picker}
    </HintBox>
  );
}
