#!/usr/bin/env bash
# Run the runic-serve Postgres API test against a throwaway Dockerized DB.
#
# Spins up postgres in Docker, waits for readiness, runs the postgres-feature
# API test with Postgres session + artifact-metadata stores and local blob
# bytes, then tears the container down.
#
# Any args are forwarded to the test harness (after `--`):
#   scripts/test-postgres.sh                 # the postgres_api suite
#   scripts/test-postgres.sh full_lifecycle  # filter by name
set -euo pipefail

CONTAINER=runic-serve-test-pg
PORT=55433
export RUNIC_TEST_DATABASE_URL="postgres://postgres:postgres@localhost:${PORT}/runic_test"

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

cleanup
echo "starting postgres ($CONTAINER) on :$PORT …"
docker run -d --name "$CONTAINER" \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=runic_test \
  -p "${PORT}:5432" postgres:16-alpine >/dev/null

echo -n "waiting for readiness "
for _ in $(seq 1 30); do
  if docker exec "$CONTAINER" pg_isready -U postgres -d runic_test >/dev/null 2>&1; then
    echo "✓"; break
  fi
  echo -n "."; sleep 1
done

cargo test -p runic-serve --features postgres --test postgres_api -- --nocapture "$@"
echo "postgres api: OK"
