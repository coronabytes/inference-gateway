import { useEffect, useId, useLayoutEffect, useRef, useState, type ComponentProps, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from "react";
import { dayLabel, utcDay } from "../format";
import { Floating, useFloating } from "./floating";
import { Caption, HintBox as WithHint, useHint } from "./hint";
import { SearchSelect } from "./SearchSelect";

export { Caption } from "./hint";

/*
 * The console's form controls. Each is a pill with its caption inside, before the value (muted,
 * behind a hairline divider). A `hint` opens as a popover when the caption is clicked (hint.tsx).
 * Build forms and filter bars from these instead of bare <input>, <textarea>, <button> or
 * hand-made segmented bars, so every control looks and behaves the same. TagEditor, TagPicker
 * (Tags.tsx) and CatalogPicker follow the same pattern for their own kinds of input.
 */

const cx = (...c: (string | false | null | undefined)[]) => c.filter(Boolean).join(" ");

/** An <input> described by the hint of the control around it. */
function Input(props: ComponentProps<"input">) {
  return <input aria-describedby={useHint()?.id} {...props} />;
}

/** Grows with its content from `rows` up, instead of scrolling or being resized by hand. */
function Textarea(props: ComponentProps<"textarea">) {
  const ref = useRef<HTMLTextAreaElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight}px`;
  }, [props.value]);
  return <textarea ref={ref} aria-describedby={useHint()?.id} {...props} />;
}

function RadioGroup(props: ComponentProps<"div">) {
  return <div aria-describedby={useHint()?.id} {...props} />;
}

/**
 * Masks secrets with CSS rather than type="password": browsers and password managers offer to
 * fill in or generate a password for every password field and ignore autocomplete="off" there,
 * which put the admin's saved master key into these fields. The data-* attributes opt out of the
 * common managers.
 */
const SECRET = {
  autoComplete: "off",
  autoCorrect: "off",
  autoCapitalize: "off",
  spellCheck: false,
  "data-1p-ignore": "",
  "data-bwignore": "",
  "data-lpignore": "true",
  "data-form-type": "other",
} as const;

/**
 * A single-line text input. `secret` masks the value (API keys, client secrets). `onClear` adds an
 * × at the end of the pill, named `clearLabel` (default "Clear {caption}"), for example to clear
 * the value or remove a stored secret.
 */
export function TextInput({
  caption,
  hint,
  mono,
  secret,
  onClear,
  clearLabel,
  className,
  ...props
}: Omit<ComponentProps<"input">, "children"> & {
  caption: string;
  hint?: ReactNode;
  mono?: boolean;
  secret?: boolean;
  onClear?: () => void;
  clearLabel?: string;
}) {
  return (
    <WithHint hint={hint} className={className}>
      <label className={cx("control", props.disabled && "disabled")}>
        <Caption>{caption}</Caption>
        <Input {...props} {...(secret && { ...SECRET, type: "text" })} className={cx((mono || secret) && "mono", secret && "secret-input")} />
        {onClear && (
          <button type="button" className="control-clear" aria-label={clearLabel ?? `Clear ${caption}`} title={clearLabel ?? "Clear"} disabled={props.disabled} onClick={onClear}>
            ×
          </button>
        )}
      </label>
    </WithHint>
  );
}

/** A multi-line text input; the caption sits on its own line at the top of the box. */
export function TextArea({ caption, hint, className, ...props }: Omit<ComponentProps<"textarea">, "children"> & { caption: string; hint?: ReactNode }) {
  return (
    <WithHint hint={hint} className={className}>
      <label className={cx("control multiline", props.disabled && "disabled")}>
        <Caption>{caption}</Caption>
        <Textarea {...props} />
      </label>
    </WithHint>
  );
}

/**
 * A dropdown (SearchSelect) with its caption inside. `label`, the accessible name, defaults to it.
 * It fills its column in a form (`.form`) and fits its content in a filter bar.
 */
export function Select({
  caption,
  hint,
  label,
  className,
  ...props
}: Omit<ComponentProps<typeof SearchSelect>, "label" | "caption"> & { caption: string; label?: string; hint?: ReactNode; className?: string }) {
  return (
    <WithHint hint={hint} className={className}>
      <SearchSelect {...props} caption={caption} label={label ?? caption} />
    </WithHint>
  );
}

const TIME_RANGES = {
  "1h": "Last hour",
  "24h": "Last 24 hours",
  "7d": "Last 7 days",
  "30d": "Last 30 days",
  month: "This month",
  last_month: "Last month",
  custom: "Custom",
} as const;

export type TimeRangeId = keyof typeof TIME_RANGES;

/** The first and last day (UTC) of the month before this one. */
function lastMonth(): [string, string] {
  const now = new Date();
  const first = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - 1, 1));
  const last = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), 0));
  return [first.toISOString().slice(0, 10), last.toISOString().slice(0, 10)];
}

/**
 * State for a `TimeRange`: spread `props` onto it. `query` is the admin API's `range` (and `from`
 * and `to` for a custom range) as a query string, to start a URLSearchParams with; it is null while
 * the custom days are incomplete or reversed. Last month is sent as a custom range. `ended` is set
 * for a range that ended before today, which no longer changes.
 */
export function useTimeRange<T extends TimeRangeId>(ranges: readonly T[], initial: T) {
  const [range, setRange] = useState<T>(initial);
  // Custom range: first and last day, inclusive, in UTC like the gateway's budgets.
  const [from, setFrom] = useState(() => utcDay(29));
  const [to, setTo] = useState(() => utcDay());
  const [first, last] = range === "last_month" ? lastMonth() : [from, to];
  const days = range === "custom" || range === "last_month";
  // A date field holds years like 0202 while its year is being typed; wait for the whole year.
  const complete = first >= "1970" && last >= "1970" && first <= last;
  const query = !days ? `range=${range}` : complete ? `range=custom&from=${first}&to=${last}` : null;
  return {
    range,
    query,
    ended: days && last < utcDay(),
    props: { ranges, value: range, onChange: setRange, custom: { from, to, onFrom: setFrom, onTo: setTo } },
  };
}

/**
 * The time range of a view, as a dropdown that shows only the active range. `ranges` are the ones
 * the view offers, in order. With `custom` among them, the dropdown ends with From and To fields
 * (UTC days); changing them picks the custom range, which the button then shows as its days.
 */
export function TimeRange<T extends TimeRangeId>({
  ranges,
  value,
  onChange,
  custom,
}: {
  ranges: readonly T[];
  value: T;
  onChange: (value: T) => void;
  custom: { from: string; to: string; onFrom: (day: string) => void; onTo: (day: string) => void };
}) {
  const offersCustom = (ranges as readonly TimeRangeId[]).includes("custom");
  const pickCustom = () => onChange("custom" as T);
  const reversed = !!custom.from && !!custom.to && custom.from > custom.to;
  return (
    <Select
      caption="Time"
      label="Time range"
      value={value}
      onChange={(v) => onChange(v as T)}
      options={ranges.filter((r) => r !== "custom").map((r) => ({ value: r, label: TIME_RANGES[r] }))}
      display={value === "custom" ? `${dayLabel(custom.from)} – ${dayLabel(custom.to)}` : undefined}
      footer={
        offersCustom && (
          <div className={value === "custom" ? "time-custom selected" : "time-custom"}>
            <div className="select-group">Custom range (UTC)</div>
            <TextInput
              caption="From"
              type="date"
              value={custom.from}
              max={custom.to || undefined}
              onChange={(e) => {
                custom.onFrom(e.target.value);
                pickCustom();
              }}
              aria-label="From (UTC)"
            />
            <TextInput
              caption="To"
              type="date"
              value={custom.to}
              min={custom.from || undefined}
              onChange={(e) => {
                custom.onTo(e.target.value);
                pickCustom();
              }}
              aria-label="To (UTC)"
            />
            {value !== "custom" && (
              <Button variant="ghost" onClick={pickCustom}>
                Show these days
              </Button>
            )}
            {reversed && <span className="time-custom-note">The range ends before it starts.</span>}
          </div>
        )
      }
    />
  );
}

/**
 * A dropdown that picks any number of options. `none` ("All" by default) is shown when nothing is
 * picked, and is offered first to clear the choice. Picking keeps the list open.
 */
export function MultiSelect({
  caption,
  hint,
  label,
  className,
  values,
  onChange,
  none = "All",
  ...props
}: Omit<ComponentProps<typeof SearchSelect>, "label" | "caption" | "value" | "onChange" | "multiple"> & {
  caption: string;
  label?: string;
  hint?: ReactNode;
  className?: string;
  values: string[];
  onChange: (values: string[]) => void;
  none?: string;
}) {
  return (
    <WithHint hint={hint} className={className}>
      <SearchSelect {...props} value="" onChange={() => {}} multiple={{ values, onChange, none }} caption={caption} label={label ?? caption} />
    </WithHint>
  );
}

/** A key that `KeyValueInput` suggests, with the values it suggests for it. */
export interface KeySuggestion {
  key: string;
  values: { value: string; note?: string }[];
}

/**
 * `key=value` pairs as chips, such as request labels. Typing offers the suggested keys; picking
 * one (or typing `=` or `:` after a key) offers that key's values. Enter, a comma or Tab adds the
 * typed pair, so keys and values that are not suggested can be typed too, and Backspace in the
 * empty input removes the last pair. A pair whose key is already there replaces it. `validate`
 * says why a typed pair cannot be added; `max` caps the number of pairs. `onOpen` runs whenever
 * the input gets the focus, e.g. to read the suggestions again.
 */
export function KeyValueInput({
  caption,
  hint,
  value,
  onChange,
  suggestions,
  validate,
  max,
  placeholder,
  onOpen,
}: {
  caption: string;
  hint?: ReactNode;
  value: Record<string, string>;
  onChange: (value: Record<string, string>) => void;
  suggestions: KeySuggestion[];
  validate?: (key: string, value: string) => string | null;
  max?: number;
  placeholder?: string;
  onOpen?: () => void;
}) {
  const [draft, setDraft] = useState("");
  const [note, setNote] = useState<string | null>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const listId = useId();
  const inputId = useId();
  const hintId = useId();
  const field = useRef<HTMLDivElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);

  // Keys are letters, digits and a few marks, so the first '=' or ':' ends the key.
  const split = (text: string): [string, string | null] => {
    const at = text.search(/[=:]/);
    return at < 0 ? [text.trim(), null] : [text.slice(0, at).trim(), text.slice(at + 1).trim()];
  };
  const [typedKey, typedValue] = split(draft);
  const full = max !== undefined && Object.keys(value).length >= max;

  /** Adds `key=val`, replacing what `key` had; false (with a note) when it cannot be added. */
  const add = (key: string, val: string, next = value) => {
    const problem = validate?.(key, val) ?? null;
    if (problem) {
      setNote(problem);
      return null;
    }
    if (!(key in next) && max !== undefined && Object.keys(next).length >= max) {
      setNote(`At most ${max} at a time.`);
      return null;
    }
    setNote(null);
    return { ...next, [key]: val };
  };

  /** Adds the complete pairs in `text`; a trailing part (without its comma) stays typed. */
  const commit = (text: string, keepLast = false) => {
    const parts = text.split(",");
    const rest = keepLast ? (parts.pop() ?? "") : "";
    let next = value;
    for (const part of parts) {
      const [k, v] = split(part);
      if (!k && !v) continue;
      if (!k || !v) {
        setNote("Type a key and a value, as key=value.");
        setDraft(part.trim());
        return false;
      }
      const added = add(k, v, next);
      if (!added) {
        setDraft(part.trim());
        if (next !== value) onChange(next);
        return false;
      }
      next = added;
    }
    if (next !== value) onChange(next);
    setDraft(rest);
    return true;
  };

  // While a key is typed: the keys not chosen yet. After its '=': that key's values.
  const lower = (s: string) => s.toLowerCase();
  const choices: { text: string; note?: string; pick: () => void }[] =
    typedValue === null
      ? suggestions
          .filter((s) => !(s.key in value) && lower(s.key).includes(lower(typedKey)))
          .map((s) => ({
            text: s.key,
            note: s.values.length === 1 ? "1 value" : `${s.values.length} values`,
            pick: () => {
              setDraft(`${s.key}=`);
              setNote(null);
            },
          }))
      : (suggestions.find((s) => s.key === typedKey)?.values ?? [])
          .filter((v) => v.value !== value[typedKey] && lower(v.value).includes(lower(typedValue)))
          .map((v) => ({ text: v.value, note: v.note, pick: () => commit(`${typedKey}=${v.value}`) }));
  const heading = typedValue === null ? "Keys" : `Values of ${typedKey}`;
  // A full filter can still change the value of a key it has.
  const listShown = open && choices.length > 0 && !(full && typedValue === null);
  useFloating(listShown, field, popup);

  const pick = (i: number) => {
    choices[i].pick();
    setActive(-1);
    input.current?.focus();
  };

  const onKeyDown = (e: ReactKeyboardEvent<HTMLInputElement>) => {
    if (listShown && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
      e.preventDefault();
      setActive((i) => (e.key === "ArrowDown" ? Math.min(choices.length - 1, i + 1) : Math.max(-1, i - 1)));
    } else if (listShown && e.key === "Escape") {
      // Close only the list, not a dialog around it.
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
    } else if (listShown && e.key === "Enter" && active >= 0) {
      e.preventDefault();
      pick(active);
    } else if (e.key === "Enter") {
      e.preventDefault();
      // A key alone moves on to its values.
      if (typedKey && typedValue === null) setDraft(`${typedKey}=`);
      else commit(draft);
    } else if (e.key === "Tab" && typedKey && typedValue) {
      if (!commit(draft)) e.preventDefault();
    } else if (e.key === "Backspace" && !draft) {
      const keys = Object.keys(value);
      if (keys.length) onChange(Object.fromEntries(Object.entries(value).filter(([k]) => k !== keys[keys.length - 1])));
    }
  };

  const remove = (key: string) => onChange(Object.fromEntries(Object.entries(value).filter(([k]) => k !== key)));

  return (
    <WithHint hint={hint} id={hintId} note={note ?? undefined}>
      <div className="tag-combo">
        <div className="tag-input pair-input" ref={field}>
          <Caption htmlFor={inputId}>{caption}</Caption>
          {Object.entries(value).map(([k, v]) => (
            <span key={k} className="tag pair" title={`${k}=${v}`}>
              <span className="pair-key">{k}</span>
              <span className="pair-sep">=</span>
              <span className="pair-value">{v}</span>
              <button type="button" aria-label={`Remove ${k}=${v}`} onClick={() => remove(k)}>
                ×
              </button>
            </span>
          ))}
          <input
            ref={input}
            id={inputId}
            className="mono"
            aria-describedby={hint ? hintId : undefined}
            role="combobox"
            aria-autocomplete="list"
            aria-expanded={listShown}
            aria-controls={listId}
            aria-activedescendant={listShown && active >= 0 ? `${listId}-${active}` : undefined}
            aria-invalid={note ? true : undefined}
            autoComplete="off"
            spellCheck={false}
            value={draft}
            placeholder={Object.keys(value).length ? undefined : placeholder}
            onChange={(e) => {
              const v = e.target.value;
              setOpen(true);
              setActive(-1);
              setNote(null);
              // Typed or pasted commas separate pairs.
              if (v.includes(",")) commit(v, true);
              else setDraft(v);
            }}
            onKeyDown={onKeyDown}
            onFocus={() => {
              setOpen(true);
              onOpen?.();
            }}
            onBlur={() => {
              setOpen(false);
              setActive(-1);
              if (typedKey && typedValue) commit(draft);
            }}
          />
        </div>
        {listShown && (
          <Floating>
            <div className="select-popup floating" ref={popup}>
              <ul id={listId} role="listbox" aria-label={heading}>
                <li className="select-group" role="presentation">
                  {heading}
                </li>
                {choices.map((c, i) => (
                  <li
                    key={c.text}
                    id={`${listId}-${i}`}
                    role="option"
                    aria-selected={i === active}
                    className={i === active ? "active mono" : "mono"}
                    onMouseEnter={() => setActive(i)}
                    // Keep the focus in the input, so picking does not blur it first.
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => pick(i)}
                  >
                    {c.text}
                    {c.note && <span className="select-note">{c.note}</span>}
                  </li>
                ))}
              </ul>
            </div>
          </Floating>
        )}
      </div>
    </WithHint>
  );
}

export interface SegmentOption<T extends string> {
  value: T;
  label: string;
  /** Shown instead of the label, which stays the choice's accessible name and tooltip. */
  icon?: ReactNode;
  disabled?: boolean;
}

/** One choice among a few (a tab-select): a pill of toggles. `children` go after the choices. */
export function Segmented<T extends string>({
  caption,
  label,
  options,
  value,
  onChange,
  hint,
  className,
  children,
}: {
  caption?: string;
  /** Accessible name; defaults to the caption. */
  label?: string;
  options: SegmentOption<T>[];
  value: T;
  onChange: (value: T) => void;
  hint?: ReactNode;
  className?: string;
  children?: ReactNode;
}) {
  return (
    <WithHint hint={hint}>
      <div className={cx("segmented", className)}>
        {caption && <Caption>{caption}</Caption>}
        <RadioGroup className="segmented-group" role="radiogroup" aria-label={label ?? caption}>
          {options.map((o) => (
            <button
              key={o.value}
              type="button"
              role="radio"
              aria-checked={value === o.value}
              className={cx(value === o.value && "active", o.icon != null && "icon-choice")}
              disabled={o.disabled}
              onClick={() => onChange(o.value)}
              {...(o.icon != null ? { "aria-label": o.label, title: o.label } : { "data-label": o.label })}
            >
              {o.icon ?? o.label}
            </button>
          ))}
        </RadioGroup>
        {children}
      </div>
    </WithHint>
  );
}

/** A page's tabs, above its content. Pair with `useHashTab` so the tab is part of the address. */
export function Tabs<T extends string>({ label, options, value, onChange }: { label: string; options: readonly SegmentOption<T>[]; value: T; onChange: (value: T) => void }) {
  return (
    <div className="tabs" role="tablist" aria-label={label}>
      {options.map((o) => (
        <button key={o.value} type="button" role="tab" aria-selected={value === o.value} className={value === o.value ? "active" : ""} onClick={() => onChange(o.value)} data-label={o.label}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** `value` once it has stopped changing for `ms`: a typed search sent to the server, not every keystroke. */
export function useDebounced<T>(value: T, ms = 250): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return settled;
}

/**
 * The selected tab of `page`, kept in the address as `#/page/tab` so that links and reloads open
 * it. The first of `tabs` is the default and has no suffix. `tabs` must not change between renders.
 */
export function useHashTab<T extends string>(page: string, tabs: readonly T[]): [T, (tab: T) => void] {
  const read = () => {
    // A tab may take parameters after it: `#/page/tab?name=value`.
    const tab = window.location.hash.replace(/^#\/?/, "").split("?")[0].split("/")[1] ?? "";
    return (tabs as readonly string[]).includes(tab) ? (tab as T) : tabs[0];
  };
  const [tab, setTab] = useState<T>(read);
  useEffect(() => {
    const onHash = () => setTab(read());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  const select = (t: T) => {
    window.location.hash = t === tabs[0] ? `#/${page}` : `#/${page}/${t}`;
  };
  return [tab, select];
}

/**
 * An on/off switch: a pill button with a sliding knob, sized to sit in a segmented bar. Without
 * `children` it is just the knob, named by `label`.
 */
export function Switch({
  checked,
  onChange,
  label,
  disabled,
  children,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label?: string;
  disabled?: boolean;
  children?: ReactNode;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      aria-describedby={useHint()?.id}
      className="switch"
      disabled={disabled}
      onClick={() => onChange(!checked)}
    >
      <span className="switch-track" aria-hidden="true" />
      {children}
    </button>
  );
}

/**
 * A Switch in a pill of its own, with an optional caption before it. With a caption the text after
 * the knob is optional: "Status ●" needs no "Enabled".
 */
export function Toggle({ caption, hint, ...props }: ComponentProps<typeof Switch> & { caption?: string; hint?: ReactNode }) {
  return (
    <WithHint hint={hint}>
      <div className="segmented">
        {caption && <Caption>{caption}</Caption>}
        <Switch label={props.children ? undefined : caption} {...props} />
      </div>
    </WithHint>
  );
}

/**
 * A button that opens the file chooser; with a `caption`, the button sits in a pill after it, like
 * the other controls. Choosing the same file again still calls `onFile`. `required` marks the pill
 * like an empty required field; pass it only while no file is chosen. With `multiple`, `onFile` is
 * called for each chosen file. `label` names a button whose children are only an icon, and
 * `className` replaces its look, e.g. "icon-btn".
 */
export function FileButton({
  caption,
  hint,
  label,
  accept,
  multiple,
  onFile,
  disabled,
  required,
  className,
  children,
}: {
  caption?: string;
  hint?: ReactNode;
  label?: string;
  accept?: string;
  multiple?: boolean;
  onFile: (file: File) => void;
  disabled?: boolean;
  required?: boolean;
  className?: string;
  children: ReactNode;
}) {
  const input = useRef<HTMLInputElement>(null);
  const button = (
    <Button className={className ?? "file-pick"} onClick={() => input.current?.click()} disabled={disabled} aria-label={label} title={label}>
      {children}
    </Button>
  );
  return (
    <WithHint hint={hint}>
      {caption ? (
        <div className={cx("segmented", required && "required-empty")}>
          <Caption>{caption}</Caption>
          {button}
        </div>
      ) : (
        button
      )}
      <input
        ref={input}
        type="file"
        accept={accept}
        multiple={multiple}
        onChange={(e) => {
          const files = [...(e.target.files ?? [])];
          e.target.value = "";
          files.forEach(onFile);
        }}
        hidden
        aria-hidden="true"
      />
    </WithHint>
  );
}

/**
 * A button with a menu of related actions joined to it, such as Send with Show curl and Clear
 * chat: one pill, the action on the left and a chevron on the right that opens the menu (see
 * `MenuButton`). The menu stays usable while the action is disabled.
 */
export function SplitButton({
  variant,
  onClick,
  disabled,
  title,
  menuLabel,
  items,
  children,
}: {
  variant?: "primary" | "ghost" | "danger";
  onClick: () => void;
  disabled?: boolean;
  title?: string;
  /** Accessible name of the chevron and its menu. */
  menuLabel: string;
  items: MenuItem[];
  children: ReactNode;
}) {
  return (
    <div className={cx("split-button", variant)}>
      <Button variant={variant} onClick={onClick} disabled={disabled} title={title}>
        {children}
      </Button>
      <MenuButton label={menuLabel} className={cx("split-toggle", variant)} items={items}>
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" aria-hidden="true">
          <path d="m6 9 6 6 6-6" />
        </svg>
      </MenuButton>
    </div>
  );
}

/** A pill button. `primary` for the main action of a dialog or page, `ghost` for quiet ones. */
export function Button({ variant, className, type = "button", ...props }: ComponentProps<"button"> & { variant?: "primary" | "ghost" | "danger" }) {
  return <button aria-describedby={useHint()?.id} {...props} type={type} className={cx(variant, className) || undefined} />;
}

export interface MenuItem {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  title?: string;
  /** Destructive, such as Delete: shown in red. */
  danger?: boolean;
  /** A choice among the menu's items: set on every item, true on the one in effect, which is ticked. */
  checked?: boolean;
}

/**
 * A button that opens a menu of actions. The menu is on its own layer (`Floating`), placed under the
 * button and aligned to its right edge (`align="left"`: its left edge), so a table or card around
 * it cannot clip it; scrolling or resizing closes it, as do a click elsewhere and Escape.
 * `className` styles the button, e.g. "icon-btn" for an icon in a row's commands column.
 */
export function MenuButton({
  items,
  label,
  className,
  align = "right",
  children,
}: {
  items: MenuItem[];
  /** Accessible name of the button, and of the menu. */
  label: string;
  className?: string;
  align?: "left" | "right";
  children: ReactNode;
}) {
  const root = useRef<HTMLDivElement>(null);
  const toggle = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const [at, setAt] = useState<{ top?: number; bottom?: number; right?: number; left?: number } | null>(null);
  const menuId = useId();

  const close = (refocus = false) => {
    setAt(null);
    if (refocus) toggle.current?.focus();
  };

  useEffect(() => {
    if (!at) return;
    const away = (e: PointerEvent) => {
      const t = e.target as Node;
      if (!root.current?.contains(t) && !menu.current?.contains(t)) close();
    };
    const key = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopImmediatePropagation();
      close(true);
    };
    const moved = () => close();
    document.addEventListener("pointerdown", away);
    window.addEventListener("keydown", key, true);
    window.addEventListener("scroll", moved, true);
    window.addEventListener("resize", moved);
    return () => {
      document.removeEventListener("pointerdown", away);
      window.removeEventListener("keydown", key, true);
      window.removeEventListener("scroll", moved, true);
      window.removeEventListener("resize", moved);
    };
  }, [at]);

  const open = () => {
    const r = toggle.current?.getBoundingClientRect();
    if (!r) return;
    // `right` of a fixed element counts from the page's edge, inside any scrollbar.
    const side = align === "left" ? { left: r.left } : { right: document.documentElement.clientWidth - r.right };
    // Above the button when there is no room for a few items below it, as at the bottom of a page.
    const room = window.innerHeight - r.bottom;
    setAt(room < 40 * items.length + 24 && r.top > room ? { bottom: window.innerHeight - r.top + 6, ...side } : { top: r.bottom + 6, ...side });
  };

  return (
    <div className="menu-button" ref={root}>
      <button
        ref={toggle}
        type="button"
        className={className}
        aria-label={label}
        title={label}
        aria-haspopup="menu"
        aria-expanded={!!at}
        aria-controls={at ? menuId : undefined}
        onClick={() => (at ? close() : open())}
      >
        {children}
      </button>
      {at && (
        <Floating>
          <div
            className="select-popup menu-popup"
            ref={menu}
            id={menuId}
            role="menu"
            aria-label={label}
            style={{ top: at.top ?? "auto", bottom: at.bottom, right: at.right ?? "auto", left: at.left ?? "auto" }}
          >
            {items.map((item) => (
              <button
                key={item.label}
                type="button"
                role={item.checked === undefined ? "menuitem" : "menuitemradio"}
                aria-checked={item.checked}
                className={item.danger ? "danger" : undefined}
                disabled={item.disabled}
                title={item.title}
                onClick={() => {
                  close();
                  item.onClick();
                }}
              >
                {item.checked !== undefined && (
                  <span className="menu-check" aria-hidden="true">
                    {item.checked ? "✓" : ""}
                  </span>
                )}
                {item.label}
              </button>
            ))}
          </div>
        </Floating>
      )}
    </div>
  );
}

export interface PageAction {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  title?: string;
  /** The page's main action: always shown as a button, never folded into "More". */
  primary?: boolean;
}

/**
 * A page's buttons, at the end of its `.page-head`. When they do not fit beside the filters, the
 * secondary ones fold into a "More" menu and the primary ones stay. The full row is measured
 * off-screen, so the choice does not flip back and forth as the buttons change width.
 */
export function PageActions({ actions }: { actions: PageAction[] }) {
  const root = useRef<HTMLDivElement>(null);
  const full = useRef<HTMLDivElement>(null);
  const [collapsed, setCollapsed] = useState(false);

  useLayoutEffect(() => {
    const el = root.current;
    const head = el?.parentElement;
    if (!el || !head) return;
    const measure = () => {
      const gap = parseFloat(getComputedStyle(head).columnGap) || 0;
      const others = [...head.children].filter((c) => c !== el).reduce((w, c) => w + c.getBoundingClientRect().width + gap, 0);
      setCollapsed((full.current?.scrollWidth ?? 0) > head.clientWidth - others);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(head);
    for (const c of head.children) if (c !== el) observer.observe(c);
    return () => observer.disconnect();
  }, [actions.length]);

  const button = (a: PageAction) => (
    <Button key={a.label} variant={a.primary ? "primary" : undefined} onClick={a.onClick} disabled={a.disabled} title={a.title}>
      {a.label}
    </Button>
  );
  const secondary = actions.filter((a) => !a.primary);
  return (
    <div className="page-actions" ref={root}>
      <div className="page-actions-measure" ref={full} aria-hidden="true" inert>
        {actions.map(button)}
      </div>
      {collapsed && secondary.length > 0 && (
        <MenuButton items={secondary} label="More actions" className="more-button">
          More
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" aria-hidden="true">
            <path d="m6 9 6 6 6-6" />
          </svg>
        </MenuButton>
      )}
      {(collapsed ? actions.filter((a) => a.primary) : actions).map(button)}
    </div>
  );
}

const BANNER_CAPTION = { critical: "Error", warning: "Warning", good: "Done" };

/**
 * A message across the page, shaped like the controls: a pill (a rounded box once it wraps) with
 * its tone's caption inside, before the text. `actions` (ghost buttons) and, with `onDismiss`, a
 * Dismiss button go at the end. An error is announced at once (role "alert"), others politely.
 */
export function Banner({
  tone = "critical",
  caption = BANNER_CAPTION[tone],
  actions,
  onDismiss,
  className,
  children,
}: {
  tone?: "critical" | "warning" | "good";
  caption?: string;
  actions?: ReactNode;
  onDismiss?: () => void;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={cx("banner", `banner-${tone}`, className)} role={tone === "critical" ? "alert" : "status"}>
      <span className="control-caption">
        <span className="banner-icon" aria-hidden="true" />
        {caption}
      </span>
      <div className="banner-text">{children}</div>
      {(actions || onDismiss) && (
        <span className="banner-actions">
          {actions}
          {onDismiss && (
            <Button variant="ghost" onClick={onDismiss}>
              Dismiss
            </Button>
          )}
        </span>
      )}
    </div>
  );
}

/**
 * A label above content that is not one control: a fieldset-like group, a banner, a list. Single
 * controls take their caption inside instead (TextInput, Select, ...).
 */
export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <div className="field">
      <span className="field-label">{label}</span>
      {children}
      {hint != null && <span className="field-hint">{hint}</span>}
    </div>
  );
}
