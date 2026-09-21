# DDSketch ratio certification (planner integration)

Planner uses `asap_sketchlib` commit `da3635a80f8f854d47b772d49d5a9e5fb6927d8e` from [sketchlib PR #141](https://github.com/ProjectASAP/asap_sketchlib/pull/141) for mapping bounds and numerical integration tests. This pin does not update Collector or backend deployments.

For valid relative-value operand bounds a and b, with b < 1 and a nonzero true denominator, the ratio bound is `(a+b)/(1-b)`. No independence assumption is needed. Sizing both DDSketch operands to `epsilon/(2+epsilon)` meets the ratio target, and compatible readouts can share one producer.

The formula alone does not establish its preconditions. `AccuracyEvidenceProvider::quantile_input_domain` receives each complete quantile operand, including its source, filter, grouping and window. A supplied `QuantileInputDomain` promises that every evaluation population is nonempty and contains only finite values in `[lower, upper]`, with at most `max_samples` samples. That count must be in `1..=2^53`, matching the pinned interpolated readout’s count limit. Its `contract` identifies the enforced source/execution contract. Observed sample ranges, historical statistics and metric names are not this proof. The default provider returns no proof.

Certification requires each domain to lie wholly within the pinned mapping's positive or negative indexable range (or be zero alone). Same-sign interpolation preserves the relative bound. Mixed-sign interpolation can cancel: for example, `[-1, 1.001]` can have estimated median zero despite a nonzero exact median. Nonzero values below the mapping minimum are counted as zero and also do not carry the relative bound.

The denominator range must exclude zero. True and perturbed quotient ranges must stay finite and outside Float64's subnormal range, with an exact zero numerator allowed. Invalid quantile parameters and missing/invalid proofs do not receive a ratio certificate. Missing proof does not suppress candidate generation: the candidate has no root guarantee and an accuracy-enforcing selection must not treat it as certified. Invalid supplied domains are rejected. These checks are conservative: an actual window may be safe even when its declared bounds cannot prove it.

The final guarantee records both input ranges and their contract identifiers. The integration layer must only provide contracts it enforces for the plan's lifetime. This change adds no runtime guard, fallback executor, or automatic proof inference, and does not change standalone DDSketch readout certification outside this ratio rule.

## Candidate generation without evidence

The default `SketchAlgorithmStrategy` permits a direct DDSketch quantile-ratio
candidate when domain evidence is absent, but leaves the root guarantee unset.
This is useful for the v1 integration path; it does not turn missing evidence
into evidence. Other approximate divisions still require their own composition
rule or remain exact.

Callers that require a certified end-to-end accuracy target must use evidence
or select another candidate. Runtime or statically enforced domain contracts
remain future work driven by observed v1 correctness needs.
