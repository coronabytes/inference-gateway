import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { Floating, useFloating } from "./floating";
import { Caption, useHint } from "./hint";

export interface SelectOption {
  value: string;
  label: string;
  /** Options with a group are listed under its heading, in the order they come. */
  group?: string;
  /** Small text after the label, such as "router". */
  note?: string;
  /** A child of the last option before it without `indent`, such as a project's key: shown indented. */
  indent?: boolean;
  /** Shown but cannot be picked, e.g. an effort the model does not take; `note` can say why. */
  disabled?: boolean;
}

/**
 * A dropdown drawn by the app rather than the browser, so it matches the console's look when
 * open; every dropdown in the console is one. Long lists get a search box at the top. Type to
 * filter, arrow keys to move, Enter to pick, Escape to close. Inside a form field it takes the
 * field's full width. The list floats on its own layer (see `Floating`), so a dialog's
 * scrolling body cannot clip it.
 */
export function SearchSelect({
  value,
  options,
  onChange,
  label,
  searchable = options.length > 8,
  disabled = false,
  invalid = false,
  empty = "Nothing to choose",
  caption,
  multiple,
  required = false,
  display,
  footer,
}: {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  /** Accessible name of the control. */
  label: string;
  /** Shown inside the button before the value; see controls.tsx. */
  caption?: string;
  /** Defaults to lists of more than eight options. */
  searchable?: boolean;
  disabled?: boolean;
  /** Marks the current value as not usable, e.g. a model that no longer exists. */
  invalid?: boolean;
  /** Shown on the button when there are no options. */
  empty?: string;
  /**
   * Pick any number of options instead of one (`value` and `onChange` are then unused). `none`
   * is shown when nothing is picked and offered first, to clear the choice. Picking keeps the
   * list open.
   */
  multiple?: { values: string[]; onChange: (values: string[]) => void; none: string };
  /** Marks the button while nothing is chosen, like an empty required text field. */
  required?: boolean;
  /** Shown on the button instead of the chosen option's label, e.g. a value set in `footer`. */
  display?: string;
  /** Under the list in the popup: fields that set a value the options do not cover. */
  footer?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  const listId = useId();
  // In a form the list is as wide as the field, and long options wrap.
  const fill = open && !!root.current?.closest(".form, .question-row");
  useFloating(open, button, popup, fill);

  const hint = useHint();
  const items = multiple ? [{ value: "", label: multiple.none }, ...options] : options;
  const isSelected = (o: SelectOption) =>
    multiple ? (o.value === "" ? !multiple.values.length : multiple.values.includes(o.value)) : o.value === value;
  const selected = multiple ? undefined : options.find((o) => o.value === value);
  const picked = multiple && options.filter((o) => multiple.values.includes(o.value));
  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return q ? items.filter((o) => `${o.label} ${o.value} ${o.group ?? ""}`.toLowerCase().includes(q)) : items;
  }, [options, query, multiple?.none]);

  const close = (refocus = true) => {
    setOpen(false);
    setQuery("");
    if (refocus) button.current?.focus();
  };

  const pick = (o: SelectOption | undefined) => {
    if (o?.disabled) return;
    if (multiple) {
      if (!o) return;
      // Toggle the option, keeping the options' order; `none` clears the choice.
      const on = o.value !== "" && !multiple.values.includes(o.value);
      multiple.onChange(o.value === "" ? [] : options.map((x) => x.value).filter((v) => (v === o.value ? on : multiple.values.includes(v))));
      return;
    }
    if (o) onChange(o.value);
    close();
  };

  // Open on the current value; the search box takes the focus.
  useEffect(() => {
    if (!open) return;
    setActive(Math.max(0, shown.findIndex(isSelected)));
    (searchable ? search.current : list.current)?.focus();
    const outside = (e: MouseEvent) => {
      const t = e.target as Node;
      if (!root.current?.contains(t) && !popup.current?.contains(t)) close(false);
    };
    document.addEventListener("mousedown", outside);
    return () => document.removeEventListener("mousedown", outside);
    // Only when opening: typing must not move the highlight back to the current value.
  }, [open]);

  useEffect(() => setActive(0), [query]);

  // Keep the highlighted option in view.
  useEffect(() => {
    list.current?.querySelector<HTMLElement>(`[data-index="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [active, open]);

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "ArrowDown") setActive((i) => Math.min(shown.length - 1, i + 1));
    else if (e.key === "ArrowUp") setActive((i) => Math.max(0, i - 1));
    else if (e.key === "Home") setActive(0);
    else if (e.key === "End") setActive(shown.length - 1);
    else if (e.key === "Enter") pick(shown[active]);
    else if (e.key === "Escape") {
      // Close only the list, not a dialog around it.
      e.stopPropagation();
      close();
    }
    else if (e.key === "Tab") close(false);
    else return;
    e.preventDefault();
  };

  let lastGroup: string | undefined;
  return (
    // A click inside must not reach a surrounding <label> (as in Field): the label would click
    // the button again and reopen the list just closed by picking.
    <div className="select" ref={root} onClick={(e) => e.preventDefault()}>
      <button
        ref={button}
        type="button"
        className={required && (multiple ? !multiple.values.length : !selected) ? "select-button required-empty" : "select-button"}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={label}
        aria-describedby={hint?.id}
        aria-invalid={invalid || undefined}
        disabled={disabled || !options.length}
        onClick={() => (open ? close() : setOpen(true))}
        onKeyDown={(e) => {
          if (!open && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
            e.preventDefault();
            setOpen(true);
          }
        }}
      >
        <span className="select-value">
          {caption && <Caption>{caption}</Caption>}
          {display ?? (picked ? (picked.length ? picked.map((o) => o.label).join(", ") : multiple.none) : selected ? selected.label : options.length ? "Choose…" : empty)}
          {selected?.note && <span className="select-note">{selected.note}</span>}
        </span>
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" aria-hidden="true">
          <path d="m6 9 6 6 6-6" />
        </svg>
      </button>
      {open && (
        <Floating>
          <div className={`select-popup floating${fill ? " fill" : ""}`} ref={popup}>
            {searchable && (
              <input
                ref={search}
                className="select-search"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                onKeyDown={onKey}
                placeholder="Search…"
                aria-label={`Search ${label.toLowerCase()}`}
                aria-controls={listId}
                aria-activedescendant={shown[active] ? `${listId}-${active}` : undefined}
              />
            )}
            <ul id={listId} ref={list} role="listbox" aria-label={label} aria-multiselectable={multiple ? true : undefined} tabIndex={-1} onKeyDown={searchable ? undefined : onKey}>
              {shown.map((o, i) => {
                const heading = o.group && o.group !== lastGroup ? o.group : null;
                lastGroup = o.group;
                return [
                  heading && (
                    <li key={`g-${heading}`} className="select-group" role="presentation">
                      {heading}
                    </li>
                  ),
                  <li
                    key={o.value}
                    id={`${listId}-${i}`}
                    data-index={i}
                    role="option"
                    aria-selected={isSelected(o)}
                    aria-disabled={o.disabled || undefined}
                    className={`${i === active ? "active" : ""} ${isSelected(o) ? "selected" : ""} ${o.indent ? "indent" : ""} ${o.disabled ? "disabled" : ""}`}
                    onMouseEnter={() => setActive(i)}
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => pick(o)}
                  >
                    <span>{o.label}</span>
                    {o.note && <span className="select-note">{o.note}</span>}
                  </li>,
                ];
              })}
              {!shown.length && <li className="select-empty">No match</li>}
            </ul>
            {footer && (
              // Clicks stay here: the root's preventDefault (React passes portal events up) would
              // stop the fields from working.
              <div className="select-footer" onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.key === "Escape" && (e.stopPropagation(), close())}>
                {footer}
              </div>
            )}
          </div>
        </Floating>
      )}
    </div>
  );
}
