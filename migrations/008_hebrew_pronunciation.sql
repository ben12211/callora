-- Hebrew pronunciation configuration is tenant-owned and read once when a call starts.
-- The composite uniqueness constraint and every application query include business_id,
-- preventing one business's terms from being resolved in another business's call.
ALTER TABLE agent_configs
  ADD COLUMN IF NOT EXISTS hebrew_pronunciation_mode text NOT NULL DEFAULT 'smart';

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'agent_configs_hebrew_pronunciation_mode_check'
  ) THEN
    ALTER TABLE agent_configs
      ADD CONSTRAINT agent_configs_hebrew_pronunciation_mode_check
      CHECK (hebrew_pronunciation_mode IN ('off', 'smart', 'strict'));
  END IF;
END
$$;

-- Deepdub is an additional execution provider; existing rows remain unchanged.
ALTER TABLE agent_configs DROP CONSTRAINT IF EXISTS agent_configs_voice_provider_check;
ALTER TABLE agent_configs
  ADD CONSTRAINT agent_configs_voice_provider_check
  CHECK (voice_provider IN ('openai', 'elevenlabs', 'cartesia', 'deepdub'));

CREATE TABLE IF NOT EXISTS pronunciation_entries (
  id uuid PRIMARY KEY,
  business_id uuid NOT NULL REFERENCES businesses(id) ON DELETE CASCADE,
  source_text text NOT NULL CHECK (char_length(source_text) BETWEEN 1 AND 200),
  normalized_text text NOT NULL CHECK (char_length(normalized_text) BETWEEN 1 AND 200),
  pronunciation text NOT NULL CHECK (char_length(pronunciation) BETWEEN 1 AND 500),
  pronunciation_type text NOT NULL CHECK (pronunciation_type IN ('ipa', 'replacement')),
  locale text NOT NULL DEFAULT 'he-IL' CHECK (char_length(locale) BETWEEN 2 AND 16),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (business_id, normalized_text, locale)
);

CREATE INDEX IF NOT EXISTS pronunciation_entries_business_locale_idx
  ON pronunciation_entries (business_id, locale, normalized_text);
