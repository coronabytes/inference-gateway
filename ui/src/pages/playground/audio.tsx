import { useEffect, useState } from "react";
import { api, type AudioSettings, type PlaygroundOptions } from "../../api";
import { Button, Select, TextInput } from "../../components/controls";
import { ErrorBanner, Modal } from "../../components/ui";
import { ANY_VOICE } from "../../voices";

type PlaygroundModel = PlaygroundOptions["models"][number];

/** Each left to the key's models. */
const AUTOMATIC: AudioSettings = { stt: "", tts: "", voice: "" };

/** The audio settings of the person signed in, kept by the gateway, and saving new ones. */
export function useAudioSettings(): [AudioSettings, (s: AudioSettings) => Promise<void>] {
  const [settings, setSettings] = useState(AUTOMATIC);
  useEffect(() => {
    let current = true;
    // Without them, the key's models do.
    api<AudioSettings>("/playground/audio-settings")
      .then((s) => current && setSettings(s))
      .catch(() => {});
    return () => {
      current = false;
    };
  }, []);
  const save = async (s: AudioSettings) => setSettings(await api<AudioSettings>("/playground/audio-settings", { method: "PUT", json: s }));
  return [settings, save];
}

/** The voices a speech model lists, without the `*` that lets others through, and whether it does. */
function voicesOf(m: PlaygroundModel | undefined) {
  const voices = m?.voices ?? [];
  const listed = voices.filter((v) => v.id !== ANY_VOICE);
  return { listed, open: !listed.length || voices.some((v) => v.id === ANY_VOICE) };
}

/**
 * The models the chat dictates and reads aloud with: the ones chosen in the settings where the key
 * may call them, else the key's first realtime transcription model (or its first transcription
 * model) and its first speech model. The voice is sent only where the speech model takes it.
 */
export function audioModels(models: PlaygroundModel[], s: AudioSettings) {
  const transcription = models.filter((m) => m.mode === "transcription");
  const speech = models.filter((m) => m.mode === "speech");
  const stt = transcription.find((m) => m.name === s.stt) ?? transcription.find((m) => m.realtime) ?? transcription[0];
  const tts = speech.find((m) => m.name === s.tts) ?? speech[0];
  const { listed, open } = voicesOf(tts);
  const voice = s.voice && (open || listed.some((v) => v.id === s.voice)) ? s.voice : undefined;
  return { stt: stt?.name, live: !!stt?.realtime, tts: tts?.name, voice };
}

/** The chat's audio settings: which models dictate and read aloud, and in which voice. */
export function AudioSettingsDialog({
  models,
  value,
  onSave,
  onClose,
}: {
  models: PlaygroundModel[];
  value: AudioSettings;
  onSave: (s: AudioSettings) => Promise<void>;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState(value);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const transcription = models.filter((m) => m.mode === "transcription");
  const speech = models.filter((m) => m.mode === "speech");
  const automatic = audioModels(models, AUTOMATIC);
  const ttsModel = speech.find((m) => m.name === audioModels(models, draft).tts);
  const { listed, open } = voicesOf(ttsModel);
  // A voice the list does not have is typed, under "Other voice…".
  const [other, setOther] = useState(!!value.voice && !listed.some((v) => v.id === value.voice));
  const voiceChoice = other ? ANY_VOICE : draft.voice;
  const set = (patch: Partial<AudioSettings>) => setDraft((d) => ({ ...d, ...patch }));

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await onSave(draft);
      onClose();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Audio settings"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={save} disabled={busy}>
            {busy ? "Saving…" : "Save"}
          </Button>
        </>
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <div className="form">
        {transcription.length > 0 && (
          <Select
            caption="Speech to text"
            hint="Dictates into the message box. A live model writes your words as you speak; any other records, then transcribes the recording. Automatic takes the key's first live model, else its first transcription model."
            value={transcription.some((m) => m.name === draft.stt) ? draft.stt : ""}
            onChange={(stt) => set({ stt })}
            options={[
              { value: "", label: "Automatic", note: automatic.stt },
              ...transcription.map((m) => ({ value: m.name, label: m.name, note: m.realtime ? "live" : undefined })),
            ]}
          />
        )}
        {speech.length > 0 && (
          <Select
            caption="Text to speech"
            hint="Reads replies aloud. Automatic takes the key's first speech model."
            value={speech.some((m) => m.name === draft.tts) ? draft.tts : ""}
            onChange={(tts) => {
              setOther(false);
              set({ tts, voice: "" });
            }}
            options={[{ value: "", label: "Automatic", note: automatic.tts }, ...speech.map((m) => ({ value: m.name, label: m.name }))]}
          />
        )}
        {ttsModel && listed.length > 0 && (
          <Select
            caption="Voice"
            hint="The voices the speech model's deployments list. Default is the first of them."
            value={voiceChoice}
            onChange={(v) => {
              setOther(v === ANY_VOICE);
              set({ voice: v === ANY_VOICE ? "" : v });
            }}
            options={[
              { value: "", label: "Default", note: listed[0].name ?? listed[0].id },
              ...listed.map((v) => ({ value: v.id, label: v.name ?? v.id, note: v.name ? v.id : undefined })),
              ...(open ? [{ value: ANY_VOICE, label: "Other voice…" }] : []),
            ]}
          />
        )}
        {ttsModel && open && (other || !listed.length) && (
          <TextInput
            caption={listed.length ? "Voice id" : "Voice"}
            hint="Passed to the provider as is. Empty for the provider's default voice."
            mono
            value={draft.voice}
            onChange={(e) => set({ voice: e.target.value.trim() })}
            placeholder="default"
          />
        )}
        <p className="muted small">Kept for you on the gateway, whichever browser you use.</p>
      </div>
    </Modal>
  );
}
