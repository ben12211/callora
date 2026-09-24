-- Callora V2 lives in its own schema. The production database still holds the legacy
-- application's tables in `public`; nothing here reads, alters or drops them.
CREATE SCHEMA IF NOT EXISTS callora_v2;

CREATE TABLE IF NOT EXISTS callora_v2.calls (
  id               uuid PRIMARY KEY,
  call_sid         text NOT NULL UNIQUE,
  business_id      text NOT NULL,
  from_number      text,
  to_number        text NOT NULL,
  started_at       timestamptz NOT NULL DEFAULT now(),
  ended_at         timestamptz,
  outcome          text,
  twilio_status    text,
  duration_seconds integer,
  final_state      jsonb
);
CREATE INDEX IF NOT EXISTS calls_business_started ON callora_v2.calls (business_id, started_at DESC);

CREATE TABLE IF NOT EXISTS callora_v2.call_turns (
  id       bigserial PRIMARY KEY,
  call_id  uuid NOT NULL REFERENCES callora_v2.calls (id) ON DELETE CASCADE,
  speaker  text NOT NULL CHECK (speaker IN ('caller', 'agent')),
  text     text NOT NULL,
  detail   jsonb,
  at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS call_turns_call ON callora_v2.call_turns (call_id, id);
CREATE INDEX IF NOT EXISTS call_turns_at ON callora_v2.call_turns (at);

CREATE TABLE IF NOT EXISTS callora_v2.action_runs (
  id          bigserial PRIMARY KEY,
  call_id     uuid NOT NULL REFERENCES callora_v2.calls (id) ON DELETE CASCADE,
  action      text NOT NULL,
  input       jsonb NOT NULL,
  result      jsonb NOT NULL,
  ok          boolean NOT NULL,
  latency_ms  integer NOT NULL,
  at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS action_runs_call ON callora_v2.action_runs (call_id, id);

CREATE TABLE IF NOT EXISTS callora_v2.handoffs (
  id       bigserial PRIMARY KEY,
  call_id  uuid NOT NULL REFERENCES callora_v2.calls (id) ON DELETE CASCADE,
  reason   text NOT NULL,
  summary  jsonb NOT NULL,
  at       timestamptz NOT NULL DEFAULT now()
);
