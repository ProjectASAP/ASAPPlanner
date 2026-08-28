---
name: asap-deprecation-migration
description: Plan or implement ProjectASAP API, data, configuration, or dependency deprecations and migrations. Use when compatibility and staged rollout matter.
---

# Deprecation and migration

Inventory producers, consumers, persisted state, compatibility promises, and
rollback constraints. Define old and new behavior and a measurable completion
condition.

Prefer staged, reversible migration: introduce compatibility, migrate consumers
and data, observe adoption, then remove the old path. Do not remove compatibility
based only on source search when runtime or external consumers may exist.

Document operator and developer actions, telemetry, failure recovery, and the
point after which rollback is no longer safe. Test mixed-version behavior where
rolling deployment can create it.
