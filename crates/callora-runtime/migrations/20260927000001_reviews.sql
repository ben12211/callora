-- What each call's agent decisions cost (tokens, by model), for the cost per call.
ALTER TABLE callora_v2.calls ADD COLUMN IF NOT EXISTS llm_usage jsonb;

-- The owner's verdict on a call, from the calls page: a bad call becomes an eval case.
CREATE TABLE IF NOT EXISTS callora_v2.call_reviews (
  call_id  uuid PRIMARY KEY REFERENCES callora_v2.calls (id) ON DELETE CASCADE,
  verdict  text NOT NULL CHECK (verdict IN ('good', 'bad')),
  note     text NOT NULL DEFAULT '',
  at       timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS utterances_at ON callora_v2.utterances (at);
