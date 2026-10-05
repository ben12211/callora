-- What the owner changes from the settings page for the whole server rather than one business
-- (the agent's model). One row per setting.
CREATE TABLE IF NOT EXISTS callora_v2.app_settings (
  key         text PRIMARY KEY,
  value       jsonb NOT NULL,
  updated_at  timestamptz NOT NULL DEFAULT now()
);
