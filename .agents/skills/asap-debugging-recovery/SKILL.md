---
name: asap-debugging-recovery
description: Diagnose ProjectASAP failures and design safe recovery. Use for unexplained errors, incidents, flaky behavior, or recovery-path work before a confirmed correctness fix exists.
---

# Debugging and recovery

Capture the symptom, environment, timing, inputs, and last known good state.
Separate observations from hypotheses and seek the smallest reproduction.

Instrument or inspect the boundary that distinguishes competing hypotheses.
Avoid broad speculative changes. Once the cause is established, preserve a
regression test or durable diagnostic signal.

For recovery actions, define affected state, idempotency, retry limits, rollback,
and the stopping condition. Never perform destructive recovery or production
mutation without explicit authorization. Record what evidence confirms recovery.
