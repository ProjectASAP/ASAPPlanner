# Accuracy models and source contracts

Planner owns estimator accuracy, parameter sizing, propagation and candidate
legality. Deployments supply source evidence and cost information through the
existing interfaces; they do not need their own HLL accuracy or sizing model.

The `asap-aware-mapping::accuracy` module groups the shared algebra and budget
allocation, estimator implementations (`hll`), source-contract integration,
and cross-consumer reconciliation (`reconciliation`). Algorithm-specific
mathematics is not a separate candidate-selection rule.

## Source evidence to a physical candidate

1. A deployment implements `AccuracyEvidenceProvider::estimator_contract` for
   the complete aggregate expression it can certify. The scope includes its
   source, filters, grouping, windows and the union of all merged panes.
2. Planner combines that contract with the query's accuracy target (or an
   allocated local target) to size the estimator.
3. Planner derives the readout guarantee from those committed parameters,
   propagates it through the existing accuracy algebra, and uses the ordinary
   candidate legality check. Cost ranking cannot override that check.

`EstimatorContract::ClassicHll` asserts classic HLL with independent uniform
bucket hashing and an enforced maximum distinct population per complete
readout. The bounded linear-counting model supports maxima from 1 to 4096 and
precisions from 4 to 18. It is not an RSE-to-normal conversion. Sampled
cardinality and ERP observed maximum error do not establish the contract.

Missing evidence preserves an unknown HLL failure probability. Invalid or
infeasible contracts cannot authorize the approximate result. Exact execution
remains available through normal planning. A contract does not apply to a
shared-grid grouping or another estimator implementation.

The evidence provider is a trust boundary: source owners must establish its
assertions, not merely copy observed statistics into them. Public Planner tests
exercise evidence-to-sizing-to-guarantee behavior without a deployment-specific
cost or accuracy model.
