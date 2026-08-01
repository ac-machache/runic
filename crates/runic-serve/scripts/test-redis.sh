#!/usr/bin/env bash
set -euo pipefail

CONTAINER=runic-serve-test-redis
PORT=56379
export RUNIC_TEST_REDIS_URL="redis://localhost:${PORT}"

cleanup() { docker rm -f -v "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

cleanup
echo "starting redis ($CONTAINER) on :$PORT …"
docker run -d --name "$CONTAINER" -p "${PORT}:6379" redis:7-alpine >/dev/null

echo -n "waiting for readiness "
for _ in $(seq 1 30); do
  if docker exec "$CONTAINER" redis-cli ping >/dev/null 2>&1; then
    echo "✓"; break
  fi
  echo -n "."; sleep 1
done

cargo nextest run -p runic-serve --features redis --profile full --test redis_stream -- "$@"
echo "runic-serve redis suite: OK"
