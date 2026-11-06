import { useEffect, useState, type Dispatch, type SetStateAction } from "react";

/** The sessionStorage keys of page filters (and the playground's settings and conversation) start
 *  with this, so that signing out drops them all. */
const PREFIX = "filter.";

function read<T>(key: string, initial: T, valid?: (v: unknown) => boolean): T {
  try {
    const raw = sessionStorage.getItem(PREFIX + key);
    if (raw === null) return initial;
    const v: unknown = JSON.parse(raw);
    // A value of another shape is from an older console: start over.
    const sameShape = typeof v === typeof initial && Array.isArray(v) === Array.isArray(initial) && (v === null) === (initial === null);
    return sameShape && (!valid || valid(v)) ? (v as T) : initial;
  } catch {
    return initial;
  }
}

/**
 * `useState` for a page's filter, kept in this tab's sessionStorage under `key` so that it survives
 * a reload. `valid` rejects a stored value the page no longer offers, such as a removed option.
 * Back at `initial`, the stored value is removed.
 */
export function useSessionState<T>(key: string, initial: T, valid?: (v: unknown) => boolean): [T, Dispatch<SetStateAction<T>>] {
  const [value, setValue] = useState<T>(() => read(key, initial, valid));
  const json = JSON.stringify(value);
  const initialJson = JSON.stringify(initial);
  useEffect(() => {
    try {
      if (json === initialJson) sessionStorage.removeItem(PREFIX + key);
      else sessionStorage.setItem(PREFIX + key, json);
    } catch {
      // Without storage, or too big for it (a conversation with files), the value lasts until the
      // page reloads. An older one is not brought back instead.
      try {
        sessionStorage.removeItem(PREFIX + key);
      } catch {
        // No storage at all.
      }
    }
  }, [key, json, initialJson]);
  return [value, setValue];
}

/** Drops every stored filter: the next one signed in may not see the same keys and projects. */
export function clearSessionFilters() {
  try {
    for (const k of Object.keys(sessionStorage)) if (k.startsWith(PREFIX)) sessionStorage.removeItem(k);
  } catch {
    // Nothing stored, then.
  }
}

/** A `valid` for a value that must be one of `options`. */
export const oneOf =
  (options: readonly unknown[]) =>
  (v: unknown): boolean =>
    options.includes(v);
