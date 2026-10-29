import { useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { Floating, useFloating } from "./floating";
import { Caption, HintBox } from "./hint";

/** Tags as the gateway stores them: trimmed, lowercase, letters, digits, '-', '_' and '.'. */
const TAG = /^[a-z0-9._-]{1,40}$/;

export function normalizeTag(raw: string): string {
  return raw.trim().toLowerCase();
}

/** Read-only tag chips, after the chips of the projects a deployment is limited to (`projects`, in a colour of their own). */
/**
 * The guardrails a key or project applies, by name, as pills; those a key gets from its project
 * (`inherited`) are dimmed. Nothing when there are none.
 */
export function GuardrailList({ ids, inherited = [], names }: { ids: string[]; inherited?: string[]; names: Map<string, string> }) {
  const own = ids.flatMap((id) => names.get(id) ?? []);
  const fromProject = inherited.filter((id) => !ids.includes(id)).flatMap((id) => names.get(id) ?? []);
  if (!own.length && !fromProject.length) return null;
  return (
    <span className="tags guardrail-tags">
      {own.map((n) => (
        <span key={n} className="tag guardrail" title="Guardrail: every request is screened by it">
          {n}
        </span>
      ))}
      {fromProject.map((n) => (
        <span key={n} className="tag guardrail inherited" title="Guardrail from the project: every request is screened by it">
          {n}
        </span>
      ))}
    </span>
  );
}

export function TagList({ tags, projects = [], empty }: { tags: string[]; projects?: string[]; empty?: string }) {
  if (!tags.length && !projects.length) return empty ? <span className="muted">{empty}</span> : null;
  return (
    <span className="tags">
      {projects.map((p) => (
        <span key={`project:${p}`} className="tag project" title="Only keys in this project may use this deployment.">
          {p}
        </span>
      ))}
      {tags.map((t) => (
        <span key={t} className="tag">{t}</span>
      ))}
    </span>
  );
}

/**
 * Free-form tag editor: Enter, comma or Tab adds the typed tag, Backspace on an empty input
 * removes the last one. `suggestions` are offered while typing.
 *
 * Rendered as its own field (not inside a `<label>`), since a label would forward clicks on its
 * text to the first chip's remove button.
 */
export function TagEditor({
  caption,
  hint,
  value,
  onChange,
  suggestions,
}: {
  caption: string;
  /** Text, or a short list (`.hint-list`) for rules that read better line by line. */
  hint?: ReactNode;
  value: string[];
  onChange: (tags: string[]) => void;
  suggestions: string[];
}) {
  const [draft, setDraft] = useState("");
  const [invalid, setInvalid] = useState(false);
  // The suggestion list, drawn like SearchSelect's rather than as a browser datalist.
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const listId = useId();
  const inputId = useId();
  const hintId = useId();

  /** Add every complete tag in `raw`; a trailing part without a comma stays in the input. */
  const add = (raw: string, keepLast = false) => {
    const parts = raw.split(",");
    const rest = keepLast ? (parts.pop() ?? "") : "";
    const tags = parts.map(normalizeTag).filter(Boolean);
    const bad = tags.find((t) => !TAG.test(t));
    if (bad) {
      setDraft(raw);
      setInvalid(true);
      return false;
    }
    const next = [...new Set([...value, ...tags])].sort();
    if (next.length !== value.length) onChange(next);
    setDraft(rest);
    setInvalid(false);
    return true;
  };

  const offered = suggestions.filter((s) => !value.includes(s));
  const matches = offered.filter((s) => s.includes(normalizeTag(draft)));
  const listShown = open && matches.length > 0;
  const field = useRef<HTMLDivElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  useFloating(listShown, field, popup, true);

  const pick = (tag: string) => {
    add(tag);
    setActive(-1);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (listShown && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
      e.preventDefault();
      setActive((i) => (e.key === "ArrowDown" ? Math.min(matches.length - 1, i + 1) : Math.max(-1, i - 1)));
    } else if (listShown && e.key === "Escape") {
      // Close only the list, not a dialog around it.
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
    } else if (listShown && e.key === "Enter" && active >= 0) {
      e.preventDefault();
      pick(matches[active]);
    } else if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      add(draft);
    } else if (e.key === "Tab" && draft.trim()) {
      if (!add(draft)) e.preventDefault();
    } else if (e.key === "Backspace" && !draft && value.length) {
      onChange(value.slice(0, -1));
    }
  };

  return (
    <HintBox hint={hint} id={hintId} note={invalid ? "Tags are up to 40 letters, digits, '-', '_' or '.'." : undefined}>
      <div className="tag-combo">
        <div className="tag-input" ref={field}>
          <Caption htmlFor={inputId}>{caption}</Caption>
          {value.map((t) => (
            <span key={t} className="tag">
              {t}
              <button type="button" aria-label={`Remove tag ${t}`} onClick={() => onChange(value.filter((x) => x !== t))}>
                ×
              </button>
            </span>
          ))}
          <input
            id={inputId}
            aria-describedby={hint ? hintId : undefined}
            role="combobox"
            aria-autocomplete="list"
            aria-expanded={listShown}
            aria-controls={listId}
            aria-activedescendant={listShown && active >= 0 ? `${listId}-${active}` : undefined}
            autoComplete="off"
            value={draft}
            aria-invalid={invalid || undefined}
            onChange={(e) => {
              const v = e.target.value;
              setOpen(true);
              setActive(-1);
              // Typed or pasted commas separate tags.
              if (v.includes(",")) add(v, true);
              else {
                setDraft(v);
                setInvalid(false);
              }
            }}
            onKeyDown={onKeyDown}
            onFocus={() => setOpen(true)}
            onBlur={() => {
              setOpen(false);
              setActive(-1);
              if (draft.trim()) add(draft);
            }}
            placeholder={value.length ? "" : "add a tag, e.g. azure"}
          />
        </div>
        {listShown && (
          <Floating>
            <div className="select-popup floating fill" ref={popup}>
              <ul id={listId} role="listbox" aria-label={`${caption} suggestions`}>
                {matches.map((s, i) => (
                  <li
                    key={s}
                    id={`${listId}-${i}`}
                    role="option"
                    aria-selected={i === active}
                    className={i === active ? "active" : ""}
                    onMouseEnter={() => setActive(i)}
                    // Keep the focus in the input, so picking does not blur it first.
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => pick(s)}
                  >
                    {s}
                  </li>
                ))}
              </ul>
            </div>
          </Floating>
        )}
      </div>
    </HintBox>
  );
}

/** Pick from existing tags only; each tag is a toggle button. */
export function TagPicker({
  caption,
  hint,
  options,
  value,
  onChange,
}: {
  caption: string;
  hint?: string;
  options: string[];
  value: string[];
  onChange: (tags: string[]) => void;
}) {
  // Tags the key holds that no deployment carries any more stay visible, so they can be removed.
  const all = [...new Set([...options, ...value])].sort();
  const hintId = useId();
  return (
    <HintBox hint={hint} id={hintId}>
      <div className="tag-picker" role="group" aria-label={caption} aria-describedby={hint ? hintId : undefined}>
        <Caption>{caption}</Caption>
        {all.map((t) => {
          const on = value.includes(t);
          return (
            <button
              key={t}
              type="button"
              className={`tag tag-toggle ${on ? "on" : ""}`}
              aria-pressed={on}
              onClick={() => onChange(on ? value.filter((x) => x !== t) : [...value, t].sort())}
              data-label={t}
            >
              {t}
            </button>
          );
        })}
        {!all.length && <span className="muted small">No tags yet. Add tags to deployments or routers on the Models page.</span>}
      </div>
    </HintBox>
  );
}
