#!/bin/sh
set -eu

attempt=1
max_attempts="${DB_STARTUP_RETRIES:-30}"
retry_delay="${DB_STARTUP_RETRY_DELAY_SECONDS:-2}"

while ! node dist/db/migrate.js; do
  if [ "$attempt" -ge "$max_attempts" ]; then
    echo "Database migrations failed after ${attempt} attempts." >&2
    exit 1
  fi

  echo "Database is unavailable; retrying migration (${attempt}/${max_attempts})..." >&2
  attempt=$((attempt + 1))
  sleep "$retry_delay"
done

node dist/db/seed.js
exec node dist/server.js
