#!/usr/bin/env bash
# Run the runic-substrate Postgres contract against a throwaway Dockerized DB.
#
# Spins up postgres in Docker, waits for readiness, runs the postgres-feature
# tests (optionally `--ignored` stress too), then tears the container down.
#
# Any args are forwarded to the test harness (after `--`), so libtest flags work:
#   scripts/test-postgres.sh                    # contract tests only
#   scripts/test-postgres.sh --include-ignored  # contract + stress
#   scripts/test-postgres.sh --ignored          # stress only
#   scripts/test-postgres.sh some_test_name     # filter by name
set -euo pipefail

CONTAINER=runic-test-pg
PORT=55432
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

cargo test -p runic-substrate --features postgres --test postgres_contract -- --nocapture "$@"
echo "postgres contract: OK"
