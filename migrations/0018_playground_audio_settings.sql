-- The playground's audio settings, per person who signs in to the console: the models it dictates
-- and reads replies aloud with, and the voice. `subject` is who: `session:<person id>`,
-- `virtual_key:<key id>`, or `master_key` and `dev` for the sign-ins without an id. Empty means
-- the key's own choice.
CREATE TABLE playground_audio_settings (
    subject TEXT PRIMARY KEY,
    stt TEXT NOT NULL DEFAULT '',
    tts TEXT NOT NULL DEFAULT '',
    voice TEXT NOT NULL DEFAULT '',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
