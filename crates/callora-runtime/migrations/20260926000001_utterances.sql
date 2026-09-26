-- The caller's audio for each utterance, with what the realtime recognizer heard: kept only
-- for calls from the numbers in AUDIO_SAMPLE_NUMBERS (the owner's test calls), to compare
-- speech recognizers on real phone audio. μ-law 8 kHz, as Twilio sends it.
CREATE TABLE IF NOT EXISTS callora_v2.utterances (
  id       bigserial PRIMARY KEY,
  call_id  uuid NOT NULL REFERENCES callora_v2.calls (id) ON DELETE CASCADE,
  heard    text NOT NULL,
  audio    bytea NOT NULL,
  at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS utterances_call ON callora_v2.utterances (call_id, id);
