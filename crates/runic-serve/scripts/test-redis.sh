#!/usr/bin/env bash
# Run the runic-serve Redis broker test against a throwaway Dockerized Redis.
set -euo pipefail

CONTAINER=runic-serve-test-redis
PORT=56379
export RUNIC_TEST_REDIS_URL="redis://localhost:${PORT}"

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

cleanup
echo "starting redis ($CONTAINER) on :$PORT …"
docker run -d --name "$CONTAINER" -p "${PORT}:6379" redis:7-alpine >/dev/null

echo -n "waiting for readiness "
for _ in $(seq 1 30); do
  if docker exec "$CONTAINER" redis-cli ping >/dev/null 2>&1; then
    echo "✓"; break
  fi
  echo -n "."; sleep 0.3
done

cd "$(dirname "$0")/../../.."
cargo nextest run -p runic-serve --test redis_broker "$@"
echo "redis broker: OK"
