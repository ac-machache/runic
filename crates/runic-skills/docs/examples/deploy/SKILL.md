---
name: deploy
description: ship a service to production safely, with verification and rollback
---
# Deploying a service

Follow these steps in order. Do not skip verification.

1. Confirm the target: service name, environment, and the exact version/sha
   being shipped. Repeat them back before acting.
2. Check current health: the service must be green before you deploy on top
   of it. A degraded service gets fixed first, not deployed over.
3. Read `checklist.md` in this skill (pass `path: "checklist.md"` to the
   view tool) and complete every pre-flight item.
4. Ship to the canary slice first. Watch error rate and latency for at
   least 10 minutes before widening.
5. Widen gradually: 5% → 25% → 100%. At each step, compare error rate and
   p95 latency against the pre-deploy baseline.
6. If any metric regresses beyond 10% of baseline: stop, roll back to the
   previous version, and report what you observed. Rolling back is never a
   failure; shipping a regression is.

After a full rollout, post a summary: version shipped, duration, metrics
before/after, and any anomalies.
