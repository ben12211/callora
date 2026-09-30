-- A price-list bot's answers, per business and question ("מ בני ברק לירושלים"): asked again
-- only once the answer is old, so the bot is asked as little as possible.
CREATE TABLE IF NOT EXISTS callora_v2.price_answers (
  business_id  text NOT NULL,
  question     text NOT NULL,
  answer       text NOT NULL,
  asked_at     timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (business_id, question)
);
