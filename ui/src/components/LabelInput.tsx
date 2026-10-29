import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "../api";
import { LABEL_EDITOR_HINT, LABEL_FILTER_HINT, labelError, MAX_LABELS, type LabelKey, type Labels } from "../labels";
import { KeyValueInput } from "./controls";

/**
 * The label keys and values of the latest labelled requests, to suggest in a label field. They
 * are read again when the field is used after a while (`refresh`), so new labels show up.
 * Empty until read, or when they cannot be.
 */
function useLabelSuggestions(): [LabelKey[], () => void] {
  const [keys, setKeys] = useState<LabelKey[]>([]);
  const read = useRef(0);
  const refresh = useCallback(() => {
    if (Date.now() - read.current < 30_000) return;
    read.current = Date.now();
    api<LabelKey[]>("/logs/labels")
      .then(setKeys)
      .catch(() => setKeys([]));
  }, []);
  useEffect(refresh, [refresh]);
  return [keys, refresh];
}

/** Request labels as key=value chips, suggested from the latest labelled requests. */
function LabelInput({ hint, value, onChange }: { hint: string; value: Labels; onChange: (labels: Labels) => void }) {
  const [keys, refresh] = useLabelSuggestions();
  const suggestions = useMemo(
    () =>
      keys.map((k) => ({
        key: k.key,
        values: k.values.map((v) => ({ value: v.value, note: v.requests === 1 ? "1 request" : `${v.requests} requests` })),
      })),
    [keys],
  );
  return (
    <KeyValueInput
      caption="Labels"
      hint={hint}
      value={value}
      onChange={onChange}
      suggestions={suggestions}
      validate={labelError}
      max={MAX_LABELS}
      onOpen={refresh}
    />
  );
}

/** A filter bar's request labels: just the requests with all of them. */
export function LabelFilter(props: { value: Labels; onChange: (labels: Labels) => void }) {
  return <LabelInput hint={LABEL_FILTER_HINT} {...props} />;
}

/** The labels to send with requests (the playground's), as `x-gateway-labels`. */
export function LabelEditor(props: { value: Labels; onChange: (labels: Labels) => void }) {
  return <LabelInput hint={LABEL_EDITOR_HINT} {...props} />;
}
