-- Per-business AI agent configuration for the Twilio Media Streams + OpenAI Realtime call path.
CREATE TABLE IF NOT EXISTS agent_configs (
  business_id uuid PRIMARY KEY REFERENCES businesses(id) ON DELETE CASCADE,
  instructions text NOT NULL CHECK (char_length(instructions) BETWEEN 1 AND 8000),
  greeting text NOT NULL CHECK (char_length(greeting) BETWEEN 1 AND 500),
  language text NOT NULL DEFAULT 'he-IL' CHECK (char_length(language) BETWEEN 2 AND 16),
  voice text NOT NULL DEFAULT 'marin' CHECK (char_length(voice) BETWEEN 1 AND 40),
  realtime_model text NOT NULL DEFAULT 'gpt-realtime-2.1' CHECK (char_length(realtime_model) BETWEEN 1 AND 80),
  enabled boolean NOT NULL DEFAULT true,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Realtime identifiers captured while a call is bridged.
ALTER TABLE calls ADD COLUMN IF NOT EXISTS twilio_stream_sid text;
ALTER TABLE calls ADD COLUMN IF NOT EXISTS openai_session_id text;
