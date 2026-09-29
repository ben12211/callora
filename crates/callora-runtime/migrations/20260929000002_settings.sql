-- What the owner changes from the settings page, per business (the dispatch desk).
CREATE TABLE IF NOT EXISTS callora_v2.business_settings (
  business_id  text PRIMARY KEY,
  settings     jsonb NOT NULL DEFAULT '{}'::jsonb,
  updated_at   timestamptz NOT NULL DEFAULT now()
);
