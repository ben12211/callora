CREATE TABLE IF NOT EXISTS businesses (
  id uuid PRIMARY KEY,
  name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
  phone_number text NOT NULL UNIQUE CHECK (phone_number ~ '^\+[1-9][0-9]{7,14}$'),
  greeting text NOT NULL CHECK (char_length(greeting) BETWEEN 1 AND 500),
  active boolean NOT NULL DEFAULT true,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS calls (
  id uuid PRIMARY KEY,
  business_id uuid NOT NULL REFERENCES businesses(id) ON DELETE RESTRICT,
  twilio_call_sid text NOT NULL UNIQUE,
  from_number text CHECK (from_number IS NULL OR from_number ~ '^\+[1-9][0-9]{7,14}$'),
  to_number text NOT NULL CHECK (to_number ~ '^\+[1-9][0-9]{7,14}$'),
  status text NOT NULL,
  direction text,
  duration_seconds integer CHECK (duration_seconds IS NULL OR duration_seconds >= 0),
  started_at timestamptz NOT NULL DEFAULT now(),
  ended_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS calls_business_id_created_at_idx
  ON calls (business_id, created_at DESC);

CREATE INDEX IF NOT EXISTS calls_to_number_created_at_idx
  ON calls (to_number, created_at DESC);
