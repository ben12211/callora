-- One card per task a call completed (a booked ride, a reported lost item): everything
-- the business needs to act on it, including the caller's number and name.
CREATE TABLE IF NOT EXISTS callora_v2.orders (
  id       bigserial PRIMARY KEY,
  call_id  uuid NOT NULL REFERENCES callora_v2.calls (id) ON DELETE CASCADE,
  card     jsonb NOT NULL,
  at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS orders_at ON callora_v2.orders (at DESC);
