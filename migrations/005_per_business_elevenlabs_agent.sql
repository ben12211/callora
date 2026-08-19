-- Per-business ElevenLabs agent.
--
-- Until now every business on this provider ran on the one agent named by
-- ELEVENLABS_AGENT_ID, and Callora only sent per-call overrides. Callora can now write a
-- business's configuration into its ElevenLabs agent, which is only safe when each
-- business owns its own: pushing to a shared agent would have one tenant overwrite
-- another's prompt.
--
-- Empty means "use the platform agent", which is exactly the previous behaviour, so rows
-- that existed before this migration keep working untouched.
ALTER TABLE agent_configs
  ADD COLUMN IF NOT EXISTS elevenlabs_agent_id text NOT NULL DEFAULT '';

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'agent_configs_elevenlabs_agent_id_check'
  ) THEN
    ALTER TABLE agent_configs
      ADD CONSTRAINT agent_configs_elevenlabs_agent_id_check
      CHECK (char_length(elevenlabs_agent_id) <= 120);
  END IF;
END
$$;
