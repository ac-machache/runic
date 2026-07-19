---
name: code-review
description: review a diff for correctness, safety, and maintainability
---
# Reviewing a diff

Review in three passes, in this order. Report findings ranked by severity,
each with the file, the line, and a concrete failure scenario.

**Pass 1 — correctness.** Trace every changed code path with a hostile eye:
boundary values, empty inputs, error branches, concurrent access. A finding
must name inputs or state that produce the wrong result — "this looks
risky" is not a finding.

**Pass 2 — safety.** Injection through any user-controlled string, secrets
in code or logs, authorization checks on every new surface, resource leaks
on early-return paths.

**Pass 3 — maintainability.** Only after the code is correct and safe:
duplication that already has a home elsewhere in the codebase, naming that
lies about behavior, tests that assert the implementation instead of the
contract.

Rules:

- Read the surrounding code before judging the diff; a change that looks
  wrong in isolation may be consistent with the file's conventions.
- Verify claims by reading callers, not by trusting names.
- If the diff is fine, say it is fine — do not manufacture findings.
