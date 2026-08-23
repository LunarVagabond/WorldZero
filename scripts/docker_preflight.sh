#!/usr/bin/env bash
# Preflight check for `make docker-up` (#111) — fails fast with a clear,
# specific "here's what you need first" message instead of letting `docker
# compose up` hit a confusing container failure or letting `server` fail
# later with an opaque Postgres/Redis connection error.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

REQUIRED_VARS=(
  WZ_POSTGRES_USER
  WZ_POSTGRES_PASSWORD
  WZ_POSTGRES_DATABASE
  WZ_POSTGRES_PORT
  WZ_REDIS_PORT
)

if [ ! -f .env ]; then
  cat <<'MSG'
Hey — we need a few things before this can spin up:

  1. Copy .env.example to .env:   cp .env.example .env
  2. Fill in WZ_POSTGRES_USER / WZ_POSTGRES_PASSWORD / WZ_POSTGRES_DATABASE
     in .env (WZ_POSTGRES_PORT/WZ_REDIS_PORT already have sane defaults)
  3. Re-run this command.

See docs/product/Getting_Started_Developers.md for details.
MSG
  exit 1
fi

set -a
# shellcheck disable=SC1091
source .env
set +a

missing=()
for var in "${REQUIRED_VARS[@]}"; do
  if [ -z "${!var:-}" ]; then
    missing+=("$var")
  fi
done

if [ ${#missing[@]} -gt 0 ]; then
  echo "Hey — .env is missing a value for:"
  for var in "${missing[@]}"; do
    echo "  - $var"
  done
  echo ""
  echo "Fill those in in .env, then re-run this command."
  exit 1
fi

if [ "${WZ_POSTGRES_HOST:-}" != "localhost" ] && [ "${WZ_POSTGRES_HOST:-}" != "127.0.0.1" ]; then
  cat <<MSG
Note: WZ_POSTGRES_HOST is currently '${WZ_POSTGRES_HOST:-<unset>}'. Once these
containers are up, point WZ_POSTGRES_HOST (and WZ_REDIS_HOST) at 'localhost'
in .env so 'make quickstart' / 'cargo run -p server' actually reach them.

MSG
fi

echo "Preflight OK — starting Postgres/Redis containers..."
