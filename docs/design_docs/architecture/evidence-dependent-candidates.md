# Evidence-dependent candidates

Audience: ASAPPlanner library integrators, especially ASAPQuery-backend.

`PlanSpace` is a space of constructible logical alternatives, not a list of
certified deployment choices. Missing external evidence must not erase a
candidate whose semantics and Post-ASAP shape are already known. It also must
not turn an unknown guarantee into a satisfied accuracy requirement.

## Candidate state

| State | Planner representation | Backend action |
|---|---|---|
| Known guarantee | `ResultGuarantee` with evaluable bound and failure probability | Check the workload target and physical feasibility. |
| Missing accuracy/domain evidence | Symbolic `BoundExpr::Unknown` or `ProbabilityExpr::Unknown`, or `guarantee: None` on a constructible summary | Inspect `ReplacementSubDAG::has_missing_accuracy_evidence()`, obtain applicable evidence or apply explicit policy; do not claim certification. |
| Missing cost | `CostModel::candidate_cost()` returns `None` (including a non-finite or negative legacy estimate) | Retain the alternative for inspection; supply a comparable cost before cost-based deployment choice. |
| Unknown runtime support | `ReplacementSubDAG::runtime_support_evidence(model)` returns `None` | Candidate remains visible; bind a concrete implementation and confirm support before deployment. |
| Known invalid evidence or impossible semantics | No candidate; where supported, a `RejectedCandidate` records the error | Do not deploy. |

`ResultGuarantee::has_unknown()` detects symbolic gaps. The candidate-level
helper also covers summaries with no guarantee model, including DDSketch-ratio
`None`. A root
`AccuracyTarget` rejects a fully known guarantee that misses the target, but
does not prune a candidate solely because a required statistic is absent under
an approximate target. An exact target does not retain an uncertified summary.
Unknown is not evidence that the target is met.
For partially known guarantees, Planner tests an optimistic floor (unknown
non-negative contributions set to zero) only to reject targets already
violated by known contributions. This floor is never exported as the
candidate's guarantee or used as a certificate.

## Where evidence enters

The Planner-side gates audited for this change are:

| Path | Missing evidence | Known invalid evidence |
|---|---|---|
| Direct DDSketch quantile ratio | Candidate with no root guarantee | Incompatible supplied domain suppresses it. |
| Hydra grouping | Symbolic shared-grid collision/failure terms | Reject with typed accuracy reason. |
| Count-ranked TopK | Symbolic interval margin or failure probability | Reject overlapping/non-finite supplied intervals. |
| HLL confidence | Symbolic failure probability | Reject a fully known unmet root target. |
| Relative-value composition | Symbolic bound when input sign is unknown | Reject known signed input for this rule. |
| Exact sum/average/extremum | Symbolic row-count probability term | Reject unsupported metric combinations. |
| Cost/rate/physical evidence | `None` cost or missing workload rate; candidate remains in `PlanSpace` | Physical/lifecycle evaluation reports unavailable or rejected evidence. |
| Mixed exact/summary operator | Unknown runtime support; candidate remains in `PlanSpace` | `Some(false)` prevents construction. |

Lifecycle deployment choices are a separate output from `PlanSpace`; their
capability/cost rejections do not erase the logical summary candidate. The
backend must still check ordinary summary family, window, and state-operation
capabilities before deployment.

- Accuracy/domain: `AccuracyEvidenceProvider` supplies quantile domains and
  propagation statistics. DDSketch ratio domains, Hydra shared-grid bounds,
  TopK margin intervals, HLL confidence, and composition row counts may be
  missing. A missing non-negative-value proof for relative-error composition
  also remains symbolic. Explicitly invalid values, such as overlapping TopK
  intervals, invalid Hydra probabilities, or known signed input for that
  composition, are not treated as unknown.
- Cost/workload: `CostModel` and workload statistics determine comparable
  resource estimates. `cost_sorted()` retains unavailable candidates for
  inspection and puts them after costed alternatives. Its numeric `costs`
  display is not an availability certificate: check `candidate_cost()`.
- Runtime capability: exact value operations use a tri-state capability hook.
  The old boolean `supports_value_operation` can disprove support, but its
  permissive default is not proof; `value_operation_support_evidence` returns
  `None` until a model explicitly confirms support with `Some(true)`.
  `global_selection()` skips unknown or denied compositions. Other summary,
  maintenance, and window implementations still require backend validation.

The default `global_selection()` skips summaries that
`has_missing_accuracy_evidence()` identifies as uncertified. Its
`materialize()` result is a selected logical plan, not an instruction to
deploy every candidate in `PlanSpace`. If no alternative is chosen at a site,
materialization retains the exact `KeepPreAsap` path. The backend can instead
inspect alternatives, apply its own evidence and policy, then choose a
physically supported one; it must not equate candidate presence with approval.
Models may explicitly opt into qualitative candidate ranking when no
comparable numeric cost exists by returning `true` from
`allow_uncosted_legacy_selection()`; the default is `false`. Such ranking is
a logical preference, not a finite cost claim.

## Backend-facing workflow

### Before and after: query text to candidate space

These examples use PromQL lowering, the built-in strategies and cost model,
an approximate accuracy target, and **no external accuracy evidence**. “Before”
means `main` immediately before this PR (which already includes #449); “after”
means this PR. They describe logical planning, not a query executed by the
backend.

| PromQL input | Before this PR | After this PR |
|---|---|---|
| `count by(job)(up)` with an ε/δ target | Hydra's shared CMS/CountSketch alternatives are absent: missing shared-grid bounds make the strategy decline the target. | Both Hydra alternatives remain in `PlanSpace` with symbolic unknown bound/probability terms. `has_missing_accuracy_evidence()` is true; default `global_selection()` does not choose either as a certified answer. |
| `entropy_over_time(m[5m])` with an ε target | The uncalibrated frequency readout has no `SummaryEstimate` candidate. | Its `SummaryEstimate` remains inspectable with `guarantee: None`. Default selection still skips it, so candidate visibility is not an accuracy certificate. |
| `quantile_over_time(0.9,data[5m]) / quantile_over_time(0.5,data[5m])` with an ε target | The uncertified direct DDSketch ratio is **already** visible because of #449. | Still visible with `guarantee: None`, and still skipped by default selection. This is a regression/control example, not a new candidate introduced by this PR. |

For the first two rows, the observable change is the alternative set delivered
to an integrating backend. It may inspect or reject those alternatives; no
newly visible unknown candidate becomes the automatic selected plan. An exact
target still excludes uncertified summaries. Supplying known-invalid Hydra
evidence (for example a failure probability of `1.5`) instead produces a
`RejectedCandidate` with a reason, not a selectable alternative.

The corresponding reproducible checks are
`cargo test -p asap-frontend-promql grouped_count_keeps_uncertified_hydra_candidates_for_backend_review`,
`cargo test -p asap-frontend-promql uncalibrated_frequency_readouts_do_not_bypass_accuracy_targets`,
and `cargo test -p asap-integration-tests ddsketch_ratio_without_domain_proof_is_uncertified`.
All three start from PromQL text and exercise frontend lowering and planning.
None runs a deployed query.

For a workload containing `quantile_over_time(0.9,data[5m]) /
quantile_over_time(0.5,data[5m])`, search with `NoAccuracyEvidence` and a root
target may expose a direct DDSketch ratio candidate. Its missing domain proof
is visible through `has_missing_accuracy_evidence()`. The backend examines the
operand windows and its own data-domain facts, checks whether the denominator
can be zero, and checks that its executor supports the chosen DDSketch
parameters. It may reject that candidate and keep exact execution. It must
not publish an accuracy guarantee merely because the candidate exists.

The same loop applies to grouped Count with Hydra and to Count-ranked TopK:
their symbolic guarantees keep them visible until shared-grid or interval
evidence is available. Re-run planning with a provider when a certified
Planner selection is needed; supplying evidence to the backend alone does
not retroactively change the guarantees stored in the existing `PlanSpace`.

Backend integration work is tracked in
[ASAPQuery-backend#752](https://github.com/ProjectASAP/ASAPQuery-backend/issues/752).
