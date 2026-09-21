-- 008 let a business select Deepdub but left the calls table on the three providers 006
-- knew about, so every Deepdub call failed to record its provider and stream identifiers.
ALTER TABLE calls DROP CONSTRAINT IF EXISTS calls_provider_check;
ALTER TABLE calls
  ADD CONSTRAINT calls_provider_check
  CHECK (provider IS NULL OR provider IN ('openai', 'elevenlabs', 'cartesia', 'deepdub'));
