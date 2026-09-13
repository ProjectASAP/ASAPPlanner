# Maintained population candidates

`MaintainedPopulationStrategy` is an opt-in rule over canonical relational IR.
It separates membership semantics from shared aggregate readouts:

```text
KeepPreAsap(input)
  -> MaintainPopulation { input, max_k, quantiles } [maintenance]
  -> ReadPopulation { Quantile(q) | TopK(k) | Sum | Count | Average } [read]
  -> optional SQL projection
```

`PopulationInput::CurrentSeries` selects each series' latest live value with
label matchers, grouping, stale retraction and the canonical five-minute lookback.
`PopulationInput::Rows` retains the full input multiset, including duplicate rows;
it carries the table scan (including predicates), value column and grouping.
Table populations do not inherit latest-value selection or lookback expiry.
The initial table rule supports non-null Float64 value columns. It preserves SQL
projections and aliases; unsupported types, nullable value columns, multi-measure
aggregates and arbitrary relational inputs need further rules.

Compatible consumers share the maintenance producer through canonical CSE.
Quantile ranks are readout parameters; TopK requests retain the largest requested
k. The full population remains available so deletion of a TopK member can promote
another. Source, predicates, value column, grouping and membership semantics are
part of state identity. The maintained population and its readouts are exact;
this operator does not imply a deletable DDSketch.

IR validation checks the maintenance input, execution phases and producer/readout
compatibility. A compiler must explicitly support the selected membership model:
remote-write current-series execution is not an implementation of table-row state.
The backend currently deploys the current-series variant; table-row deployment
requires a complete row-update/deletion executor. Existing SQL summary compilation
continues separately.

Temporal sketch rules remain SummaryAgg/SummaryEstimate DAGs. UnivMon can expose
cardinality, frequency L2 and entropy readouts over the same unit-frequency input.
Sharing requires matching input, partitioning, windows and sketch parameters;
accuracy admission remains readout-specific. Entropy evidence cannot certify L2
or cardinality. State sharing alone does not establish an error bound or cost win.

The rule does not parse query text, choose a deployment, assign memory limits or
claim performance gains. Compilers lower supported typed DAGs and price complete
maintenance and readout costs before installation.
