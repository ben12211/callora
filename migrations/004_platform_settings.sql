-- Platform settings managed from the dashboard.
--
-- Every key here is the name of an environment variable the deployment already
-- understands, so nothing new has to be learned: a row simply overrides the value the
-- process started with. An absent row means "use the environment", which is exactly how
-- the deployment behaved before this table existed.
--
-- Secret values (`secret = true`) are stored sealed with AES-256-GCM under SECRETS_KEY
-- and are never returned to a browser; non-secret values are stored as written.
CREATE TABLE IF NOT EXISTS platform_settings (
  key text PRIMARY KEY CHECK (char_length(key) BETWEEN 1 AND 80),
  value text NOT NULL,
  secret boolean NOT NULL DEFAULT false,
  updated_at timestamptz NOT NULL DEFAULT now()
);
