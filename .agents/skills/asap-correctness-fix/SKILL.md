---
name: asap-correctness-fix
description: Diagnose and fix correctness bugs in ProjectASAP code using regression-first testing. Use for wrong, inconsistent, or unexpectedly failing behavior; not for performance-only changes or new features.
---

# Correctness fixes

Establish the incorrect observable behavior and identify the violated invariant.
Explain the root cause before changing production code when practical.

Add the smallest regression test that expresses the expected behavior. Confirm
that it fails for the original implementation; do not weaken assertions or
change the test merely to make it pass.

Implement the minimally complex fix. Avoid unrelated cleanup and new abstraction
unless the fix concretely requires it.

Run the focused regression test, related unit tests, and the applicable
end-to-end test. Report the cause, fix, and verification concisely.
