# UnivMon frequency summaries

> Status: partially implemented design. Planner has UnivMon types, candidate
> construction, shared readouts and accuracy-gating tests. Approximate readouts
> still require calibrated accuracy evidence; runtime compatibility and performance
> are not established by planner tests. See [candidate integration tests](../../../crates/frontend-promql/tests/univmon_candidates.rs).

This contract is for Planner and runtime developers implementing shared
frequency statistics. One UnivMon state describes the frequency vector of
input values in one population and time window. For samples `[2, 2, 5]`, the
vector is `{2: 2, 5: 1}`. Updates use the value as the item and unit weight.

| Query or intent | Readout | Meaning for the example |
| --- | --- | --- |
| `distinct_over_time` / Cardinality | Cardinality | 2 distinct values |
| `count_over_time` / Count | PointCount with no lookup value | 3 samples |
| `l2_over_time` / FrequencyL2 | FrequencyL2 | sqrt(5) |
| `entropy_over_time` / FrequencyEntropy | FrequencyEntropy | about 0.9183 bits |

The entropy and L2 names are ProjectASAP parser extensions with the frequency
semantics above. This does not establish compatibility with an external
VictoriaMetrics build. In particular, the norm of the numeric sample vector
would be sqrt(33), which is a different operation and must not use this
readout. External differential tests must establish the engine's semantics.

The `asap_sketchlib::UnivMon` implementation supplies the distinct count, L1,
L2 and entropy, each a G-sum of the paper's Algorithm 2. Its entropy uses log
base 2. Its L1 counter is exact for nonnegative updates. The parameter
contract records heap size, sketch rows, sketch columns, number of layers and
the relative error it was sized for. The Planner's accuracy rule is the
paper's guarantee only (Braverman–Ostrovsky, arXiv:1011.2571, Algorithm 4.1,
Lemma 3.8, Theorem 4.2): L2 and the distinct count are within relative error
ε with failure probability 0.3 when the layers cover the source's distinct
bound, the heap holds `φ³/ε²` keys and the Count Sketch estimates the heap's
heavy keys within `(1 ± ε)`. A target with a failure probability below 0.3
is not met. Entropy has no guarantee: a deployment accuracy model must supply
calibrated evidence before it can satisfy an accuracy target. Without that,
the Planner keeps the exact sub-DAG. HLL/Theta/KMV remain
cardinality alternatives, and exact count remains the cheaper first count
candidate.

All four readouts have the same unit-weight update, input sub-DAG, grouping,
window, parameter identity and state schema
([ASAP primitive schema](asap-primitive-schema.md)). Existing post-ASAP structural
sharing can therefore intern their state producer while preserving distinct
readout nodes. Sharing is only legal within the same execution/data scope.
Precompute placement, SummaryCatalog installation, retention, and runtime
payload ownership remain backend responsibilities.

The backend must canonicalize numeric keys consistently with exact distinct
semantics (including equal positive and negative zero), omit stale/NaN
samples according to the query language, and preserve empty-window behavior.
Neither parser acceptance nor a structural sharing test demonstrates runtime
accuracy or performance benefits.
