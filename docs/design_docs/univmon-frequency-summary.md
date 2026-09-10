# UnivMon frequency summaries

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

The existing `asap_sketchlib::UnivMon` implementation supplies `calc_card`,
`calc_l1`, `calc_l2`, and `calc_entropy`. Its entropy uses log base 2. Its L1
counter is exact for nonnegative updates; the other estimators are heuristic.
The parameter contract records heap size, sketch rows, sketch columns and
number of layers. Default dimensions define a candidate configuration, not
an epsilon guarantee. A deployment accuracy model must supply calibrated
evidence before an approximate readout can satisfy an accuracy target. Without
that evidence, the Planner keeps the exact subtree. HLL/Theta/KMV remain
cardinality alternatives, and exact count remains the cheaper first count
candidate.

All four readouts have the same unit-weight update, input subtree, grouping,
window, parameter identity and state schema. Existing post-ASAP structural
sharing can therefore intern their state producer while preserving distinct
readout nodes. Sharing is only legal within the same execution/data scope.
Precompute placement, SummaryCatalog installation, retention, and runtime
payload ownership remain backend responsibilities.

The backend must canonicalize numeric keys consistently with exact distinct
semantics (including equal positive and negative zero), omit stale/NaN
samples according to the query language, and preserve empty-window behavior.
Neither parser acceptance nor a structural sharing test demonstrates runtime
accuracy or performance benefits.
