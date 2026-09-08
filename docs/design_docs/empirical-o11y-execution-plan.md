# Offline evidence and o11y planning evaluation

Audience: developers reproducing issue #322 and the planner/control-plane evaluation.

## Scope

Use offline sketch-bench CPU, elapsed time, state/memory, disk (when measured),
and errors against offline ground truth. No runtime ground truth, posterior
feedback, or self-estimated accuracy is required. Offline accuracy observations
do not establish formal guarantees on unseen distributions.

## Execution and ownership

1. Pin isolated planner and control-plane worktrees from fetched main branches.
   Preserve existing worktrees, including unresolved cost-model changes.
2. Evidence agent: implement the versioned artifact, validation, compatibility
   matching, public cost-model integration, and fallback/decision tests.
3. Benchmark agent: run actual sketch-bench algorithms on uniform and Zipf
   inputs, preserve raw output, export artifacts with source and environment
   provenance, and document reproducible commands.
4. Replay agent: run the existing o11y PromQL corpus through the backend parser,
   its ASAPPlanner call, and the backend typed binder (not a planner-only entry),
   export binding/fallback coverage and planning latency, and separately identify
   supplemental sketch workloads.
5. Integration owner: connect compatible evidence to the downstream control
   plane, validate dependency compatibility, review other agents' changes, run
   integration checks, and report evidence-supported comparisons.

Steps 2–4 run concurrently after agreeing the artifact contract. Integration
uses their completed interfaces and measurements. Query-pattern changes are
limited to demonstrated blockers with regression coverage; unsupported query
semantics remain explicit in the report.

## Acceptance

- Versioned schema and example; explicit units, algorithm/configuration,
  dataset/distribution, environment, collection/validity times, sample count,
  dispersion, and benchmark/model provenance.
- At least two actual sketch algorithms measured offline on uniform and skewed
  inputs. CPU is distinct from elapsed time; serialization size is distinct
  from heap memory and disk I/O. Unmeasured fields stay unavailable.
- Matching evidence affects a public planning cost/ranking/lifecycle boundary.
  Tests cover changed decisions, missing/stale/mismatched evidence, invalid
  values, and unchanged accuracy guarantees.
- The planner and control-plane evaluation preserve exact fallbacks and expose
  unsupported shapes, measured provenance, and limits on benefit estimates.
- Reproducible measurement and replay commands, raw machine-readable results,
  and a concise report distinguish actual measurements from modeled totals.

## Comparison rules

Compare equivalent tasks, data, parameters, windows, horizons, and evaluation
cadences. Whole-plan estimates include build/update/readout, retained windows,
sharing, and raw residual work where evidence exists. Missing raw baseline or
physical evidence means an unavailable whole-plan speedup, not zero cost.
Microbenchmark algorithm comparisons are labeled separately. Disk usage and
network savings require their own measurements or explicit models.

Repeated-query break-even can be computed only when compatible exact baseline,
sketch build/maintenance, and readout measurements are present. It is an offline
estimate and does not establish deployed end-to-end latency improvement.
