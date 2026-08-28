---
name: asap-security-hardening
description: Review or improve ProjectASAP security boundaries and misuse resistance. Use for authentication, authorization, secrets, untrusted input, dependency, or threat-model work.
---

# Security hardening

Define assets, trust boundaries, attacker capabilities, entry points, and likely
failure impact. Prioritize reachable risks over generic checklists.

Apply least privilege, explicit authorization, safe parsing, secret hygiene, and
secure failure defaults. Preserve compatibility only when it does not retain the
vulnerability. Avoid exposing exploit details beyond what remediation and review
require.

Add tests for the security invariant and negative cases. Document residual risk,
deployment considerations, and any credential rotation or operational action
that requires human authorization.
