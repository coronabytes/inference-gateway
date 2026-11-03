-- Audio models: speech to text (/v1/audio/transcriptions) and text to speech (/v1/audio/speech).
ALTER TABLE deployments DROP CONSTRAINT deployments_mode_check;
ALTER TABLE deployments ADD CONSTRAINT deployments_mode_check
    CHECK (mode IN ('chat', 'embedding', 'decision', 'rerank', 'transcription', 'speech'));

ALTER TABLE deployments
    -- Transcription models priced by the length of their input audio (Whisper, Voxtral). Those that
    -- report tokens (gpt-4o-transcribe) are priced per token instead.
    ADD COLUMN cost_per_audio_minute DOUBLE PRECISION CHECK (cost_per_audio_minute >= 0),
    -- Speech models, priced by the characters of their input text.
    ADD COLUMN cost_per_m_characters DOUBLE PRECISION CHECK (cost_per_m_characters >= 0);

-- Audio a transcription request was billed for (seconds), and the characters a speech request
-- spoke. A routed request's entry sums its calls'.
ALTER TABLE request_logs
    ADD COLUMN audio_seconds BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN characters BIGINT NOT NULL DEFAULT 0;
