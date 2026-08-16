-- Push 1 control plane: admin authentication, per-agent voice provider selection,
-- and an audit trail for administrative changes.

-- Each business now picks its own execution provider. The platform-wide VOICE_PROVIDER
-- environment variable only supplies the default for rows created before this migration.
ALTER TABLE agent_configs
  ADD COLUMN IF NOT EXISTS voice_provider text NOT NULL DEFAULT 'openai';

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'agent_configs_voice_provider_check'
  ) THEN
    ALTER TABLE agent_configs
      ADD CONSTRAINT agent_configs_voice_provider_check
      CHECK (voice_provider IN ('openai', 'elevenlabs', 'cartesia'));
  END IF;
END
$$;

-- The `voice` field is now provider-scoped: an OpenAI voice name, an ElevenLabs voice
-- id, or a Cartesia Sonic voice UUID — and blank, meaning "keep the provider's own
-- configured voice", is a valid choice for the two providers that have one. The original
-- 1..40 check predates all of that.
ALTER TABLE agent_configs DROP CONSTRAINT IF EXISTS agent_configs_voice_check;

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'agent_configs_voice_length_check'
  ) THEN
    ALTER TABLE agent_configs
      ADD CONSTRAINT agent_configs_voice_length_check CHECK (char_length(voice) <= 80);
  END IF;
END
$$;

-- Backfill: every agent that existed before this migration ran on the deployment's
-- platform-wide VOICE_PROVIDER, so that is what it must keep running on. The migration
-- runner publishes it as `callora.default_voice_provider`; an empty setting means a
-- fresh database, where there is nothing to backfill anyway.
--
-- `voice` is cleared for the non-OpenAI providers because the stored value was an OpenAI
-- voice name, which means nothing to ElevenLabs or Cartesia. Blank restores exactly the
-- pre-upgrade behaviour: keep the voice already configured on the provider side.
UPDATE agent_configs
SET voice_provider = COALESCE(NULLIF(current_setting('callora.default_voice_provider', true), ''), 'openai'),
    voice = CASE
      WHEN COALESCE(NULLIF(current_setting('callora.default_voice_provider', true), ''), 'openai') = 'openai'
        THEN voice
      ELSE ''
    END,
    -- The Cartesia pipeline reasons with a chat model, not a realtime one, so a realtime
    -- snapshot left in this column would be sent to an endpoint that cannot serve it.
    realtime_model = CASE
      WHEN COALESCE(NULLIF(current_setting('callora.default_voice_provider', true), ''), 'openai') = 'cartesia'
        THEN COALESCE(NULLIF(current_setting('callora.default_text_llm_model', true), ''), 'gpt-4o-mini')
      ELSE realtime_model
    END;

-- Dashboard operators. Passwords are stored as scrypt hashes; the plaintext never
-- reaches the database or the logs.
CREATE TABLE IF NOT EXISTS admin_users (
  id uuid PRIMARY KEY,
  email text NOT NULL UNIQUE CHECK (char_length(email) BETWEEN 3 AND 320),
  name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
  password_hash text NOT NULL,
  active boolean NOT NULL DEFAULT true,
  last_login_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Server-side sessions. Only the SHA-256 of the cookie value is stored, so a database
-- read cannot be replayed as a login.
CREATE TABLE IF NOT EXISTS admin_sessions (
  id uuid PRIMARY KEY,
  admin_user_id uuid NOT NULL REFERENCES admin_users(id) ON DELETE CASCADE,
  token_hash text NOT NULL UNIQUE,
  csrf_token text NOT NULL,
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS admin_sessions_admin_user_id_idx ON admin_sessions (admin_user_id);
CREATE INDEX IF NOT EXISTS admin_sessions_expires_at_idx ON admin_sessions (expires_at);

-- Append-only history of important administrative changes.
CREATE TABLE IF NOT EXISTS audit_events (
  id uuid PRIMARY KEY,
  actor_id uuid REFERENCES admin_users(id) ON DELETE SET NULL,
  actor_label text NOT NULL,
  action text NOT NULL CHECK (char_length(action) BETWEEN 1 AND 80),
  entity_type text NOT NULL CHECK (char_length(entity_type) BETWEEN 1 AND 40),
  entity_id text,
  summary text NOT NULL CHECK (char_length(summary) BETWEEN 1 AND 500),
  details jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS audit_events_created_at_idx ON audit_events (created_at DESC);
CREATE INDEX IF NOT EXISTS audit_events_entity_idx ON audit_events (entity_type, entity_id, created_at DESC);
