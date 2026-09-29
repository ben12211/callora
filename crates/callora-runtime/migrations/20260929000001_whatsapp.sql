-- WhatsApp: the accounts' sending pace, where each account sends, and the messages waiting to
-- be sent (one at a time per account, paced, retried until sent). The accounts themselves and
-- their logins live in the WhatsApp service.
CREATE TABLE IF NOT EXISTS callora_v2.whatsapp_accounts (
  id        text PRIMARY KEY,
  settings  jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS callora_v2.whatsapp_targets (
  id          bigserial PRIMARY KEY,
  account_id  text NOT NULL,
  chat_id     text NOT NULL,
  chat_name   text NOT NULL,
  kind        text NOT NULL CHECK (kind IN ('group', 'contact')),
  events      text[] NOT NULL DEFAULT ARRAY['order', 'order_verify'],
  created_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE (account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS callora_v2.whatsapp_outbox (
  id               bigserial PRIMARY KEY,
  account_id       text NOT NULL,
  chat_id          text NOT NULL,
  chat_name        text NOT NULL,
  event            text NOT NULL,
  text             text NOT NULL,
  status           text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'sent', 'failed')),
  attempts         integer NOT NULL DEFAULT 0,
  last_error       text,
  created_at       timestamptz NOT NULL DEFAULT now(),
  next_attempt_at  timestamptz NOT NULL DEFAULT now(),
  sent_at          timestamptz
);
CREATE INDEX IF NOT EXISTS whatsapp_outbox_due ON callora_v2.whatsapp_outbox (account_id, status, next_attempt_at);
CREATE INDEX IF NOT EXISTS whatsapp_outbox_sent ON callora_v2.whatsapp_outbox (account_id, sent_at);
CREATE INDEX IF NOT EXISTS whatsapp_outbox_created ON callora_v2.whatsapp_outbox (created_at DESC);
