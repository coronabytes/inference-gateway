import type { Provider, Voice } from "../api";
import { PROVIDER_LABEL } from "../providers";
import { ANY_VOICE, presetVoices } from "../voices";
import { Button, TextInput, Toggle } from "./controls";
import { Badge, IconButton, TrashIcon } from "./ui";

/** A voice as the editor holds it: an id and a name, either possibly empty while typed. */
export interface VoiceRow {
  id: string;
  name: string;
}

/** The editor's state from a deployment's voices: the rows, and whether `*` lets others through. */
export function voiceRows(voices: Voice[]): { rows: VoiceRow[]; others: boolean } {
  return {
    rows: voices.filter((v) => v.id !== ANY_VOICE).map((v) => ({ id: v.id, name: v.name ?? "" })),
    others: voices.some((v) => v.id === ANY_VOICE),
  };
}

/** The voices to save: the rows with an id, then `*` when others go through. None listed is any voice. */
export function savedVoices(rows: VoiceRow[], others: boolean): Voice[] {
  const listed: Voice[] = rows
    .filter((r) => r.id.trim())
    .map((r) => (r.name.trim() ? { id: r.id.trim(), name: r.name.trim() } : { id: r.id.trim() }));
  return listed.length && others ? [...listed, { id: ANY_VOICE }] : listed;
}

/** Ids listed more than once, which the gateway refuses. */
export function duplicateVoices(rows: VoiceRow[]): Set<string> {
  const ids = rows.map((r) => r.id.trim()).filter(Boolean);
  return new Set(ids.filter((id, i) => ids.indexOf(id) !== i));
}

/**
 * A speech deployment's voices: one row each, with the id the provider knows and the name the
 * playground shows. The first is the default; any other can be made it. `others` lets voices not
 * listed through. A provider's presets can be added at once.
 */
export function VoiceEditor({
  provider,
  rows,
  others,
  onChange,
}: {
  provider: Provider;
  rows: VoiceRow[];
  others: boolean;
  onChange: (rows: VoiceRow[], others: boolean) => void;
}) {
  const dupes = duplicateVoices(rows);
  const presets = presetVoices(provider);
  const set = (next: VoiceRow[]) => onChange(next, others);
  const patch = (i: number, p: Partial<VoiceRow>) => set(rows.map((r, j) => (j === i ? { ...r, ...p } : r)));
  const addPresets = () => {
    const have = new Set(rows.map((r) => r.id.trim()));
    set([
      ...rows.filter((r) => r.id.trim() || r.name.trim()),
      ...presets.filter((p) => !have.has(p.id)).map((p) => ({ id: p.id, name: p.name ?? "" })),
    ]);
  };

  return (
    <fieldset>
      <legend>Voices</legend>
      {rows.length === 0 ? (
        <p className="muted small">
          None listed: requests may name any voice, and those that name none get the provider&apos;s default. List voices to offer them in
          the playground and to refuse others.
        </p>
      ) : (
        <div className="voice-rows">
          {rows.map((r, i) => (
            <div key={i} className="voice-row">
              <TextInput
                caption="Voice"
                hint="The id the provider knows the voice by, such as voice_478 (Mistral) or alloy (OpenAI)."
                mono
                value={r.id}
                onChange={(e) => patch(i, { id: e.target.value })}
                aria-label={`Voice ${i + 1} id`}
                aria-invalid={dupes.has(r.id.trim()) || undefined}
                placeholder="voice id"
                required
              />
              <TextInput
                caption="Name"
                hint="What the playground shows, such as Sheila (English, US). Optional."
                value={r.name}
                onChange={(e) => patch(i, { name: e.target.value })}
                aria-label={`Voice ${i + 1} name`}
                placeholder="optional"
              />
              {i === 0 ? (
                <span className="voice-default" title="Requests that name no voice get this one">
                  <Badge tone="good">default</Badge>
                </span>
              ) : (
                <Button
                  variant="ghost"
                  className="voice-default"
                  title="Requests that name no voice get this one"
                  onClick={() => set([r, ...rows.filter((_, j) => j !== i)])}
                >
                  Make default
                </Button>
              )}
              <IconButton label={`Remove voice ${i + 1}`} title="Remove" onClick={() => set(rows.filter((_, j) => j !== i))}>
                <TrashIcon />
              </IconButton>
            </div>
          ))}
        </div>
      )}
      {dupes.size > 0 && (
        <p className="small" role="alert">
          Each voice is listed once: {[...dupes].join(", ")}.
        </p>
      )}
      <div className="voice-actions">
        {rows.length > 0 && (
          <Toggle
            caption="Other voices"
            hint="Requests for voices not listed go to the provider as they are. Off, they are refused."
            checked={others}
            onChange={(on) => onChange(rows, on)}
          />
        )}
        <Button variant="ghost" onClick={() => set([...rows, { id: "", name: "" }])} disabled={rows.length >= 500}>
          Add voice
        </Button>
        {presets.length > 0 && (
          <Button variant="ghost" onClick={addPresets}>
            Add {PROVIDER_LABEL[provider]}&apos;s voices
          </Button>
        )}
      </div>
    </fieldset>
  );
}
