# Accuracy model design

## Purpose and ownership

The accuracy model answers whether a candidate computation can satisfy the
query's accuracy requirement, and what parameters and evidence it needs to do
so. Its unit of reasoning is the result of a computation, including the errors
of its inputs. A locally accurate sketch does not automatically make an entire
query accurate.

Planner owns the interpretation of accuracy targets, estimator models, error
propagation, readout requirements and candidate legality. Deployments provide
source contracts and evidence with a defensible scope. Cost models compare
resource costs; runtime implementations execute the selected semantics and
must honor the conditions attached to them.

This design applies to exact and approximate computation, composed queries and
shared DAGs. HLL is one estimator within this design, not a separate selection
system. Ingestion time and query time change where computation runs, not what
its accuracy guarantee means.

## Requirements, guarantees and evidence

These three concepts have different roles:

| Concept | Meaning | Owner |
|---|---|---|
| Accuracy target | What the caller requires of the result | Query requirements and aggregate intent |
| Result guarantee | What a particular computation can establish | Estimator models and error propagation in Planner |
| Evidence | Facts or trusted contracts needed to establish that guarantee | Source or deployment evidence provider |

`AccuracyTarget` is the authoritative requirement type:

- `AccuracyTarget::exact()` requires zero error and zero failure probability.
- `AccuracyTarget::max_error(ε)` constrains the error magnitude in the relevant metric; it does not specify a failure-probability budget.
- `.failure_probability(δ)` adds a failure-probability bound, so the target constrains both error magnitude and failure probability.
- `.accuracy_metric(m)` optionally fixes the metric ε is stated in; when it is unset, a guarantee in any metric that meets ε and δ is accepted.
- `.accuracy_scope(s)` sets which output values the (ε, δ) statement covers at once (below); the default is the whole result.

The target at a query root applies to the values its scope covers. An
aggregate's own target guides its local candidate construction. Nested local
targets do not replace the root requirement: the composed result must satisfy
it as well.

### Scope

A scope says which values of one evaluation's result must all be within ε at
once, with probability at least 1 − δ:

| `AccuracyScope` | JSON | Values covered (`V`) |
|---|---|---|
| `PerValue` | `"per_value"` | Each value on its own: `V = 1` |
| `WholeResult` (default) | `"whole_result"` | Every value of the result: `V = G`, the values one evaluation returns; unbound when neither every group key's distinct values nor, for a PromQL instant vector, the series count is declared, and always for a range vector and an ungrouped SQL result |
| `GroupsWithRecordShareAtLeast(share)` | `{"groups_with_record_share_at_least": 0.05}` | Every group (result row) holding at least `share` of the records the query aggregates in one evaluation: `V = min(⌊1 / share⌋, G)`, bound even when `G` is not; a smaller group is not covered |

The record-share scope is for queries that watch only their large groups, for
example an anomaly detector reading per-group distinct counts. `share` is a
fraction in (0, 1]; JSON rejects any other value, and one built in Rust
covers an unbound number of values, so its target stays unmet. More
than `⌊1 / share⌋` groups cannot each hold `share` of the records, so at most
100 groups qualify at 1% and 20 at 5%, whatever the number of groups. When a
row reads several aggregations, its group must hold the share of each one's
records. Groups below the share are not covered, so a guarantee over a
share meets a target over a larger share but not a per-value target, and only
a per-value guarantee is restated over a scope.

A scope bounds only the failure probability; the error bound is unchanged.
Every estimator produces a per-value guarantee (for a grouped
query, one per group: a Kmv or HLL distinct count per group, a KLL or DDSketch
quantile per group, a Count-Min or Count Sketch frequency per key, a Hydra
estimate per subpopulation; an exact value fails with probability 0). Stated
over a scope, a per-value failure probability `δ` becomes `V · δ` by the union
bound, without assuming independence, so a sketch shared by several groups
(Hydra, a keyed Count-Min) is covered the same way
(`AccuracyScope::max_values_covered` gives `V`). A guarantee whose own failure probability is
unknown (HLL without a distinct bound) serves no query with an accuracy target
(`RootPruneReason::NoGuarantee`), and a top-k readout is admitted on its
heap size and its value guarantee alone (the contracts table).

Pass 2 gives each sketch readout the error and per-value failure probability
`δ / min(R, V · fan_in)` it must meet
(`AccuracyTarget::failure_probability_per_readout_value`, the limit of the
bound `AccuracyTarget::failure_probability_bound` names split over the
estimates that bound counts): the statement
fails only when a readout value one of its `V` covered output values depends
on does. `R` is the readout values the query reads per evaluation (the
values per evaluation of every readout group below it, summed), and `fan_in`
the most readout values one output value depends on: 1 when no reduction
lies above the readout, the reduction's input values under one (all of `R`
for a global `max`; for `sum by (job)` too, since the declared facts bound
how many jobs there are, not how many series one job holds), the sum over
the sides of a binary operator, the larger side of `or` or input of a
concatenation. A top-k readout chooses its output keys by their estimates,
so each of its output values depends on every item it ranks, and its `R`
counts those items, not its output values
(`Memo::max_estimates_per_evaluation`): at most its states' count times the
distinct items one state counts (`SourceFacts::max_distinct_per_evaluation`),
and at most the rows its values come from. A readout
group whose candidates include a reduction over readouts below it takes the
larger of the two counts. Both counts are per evaluation, not per subquery
step. An unknown term drops out of the `min`; with both unknown no per-value
failure probability follows. At δ = 0.01, `quantile_over_time(0.99, m[5m])`
over 8 series per value keeps 0.01 (min(8, 1 · 1)), and over its whole
result gets 0.01 / 8; a `max` over 1000 readouts returns one value yet fails
when any readout does, so it gets 0.01 / 1000 in any scope; and `max by
(job) (q or r)` splits over the readouts of both sides. Pass 1's candidate
takes the first requirement level and Pass 2 adds a copy per further level.
A readout several queries reach gets the union of their levels, less each
level the listed builds that meet it are exactly those that meet a stricter
one: DDSketch's relative error does not depend on δ, so of (5%, 0.01) and
(5%, 1e-4) only the second is kept. Pass 2 sizes nothing: a state has one
build per listed θ, and each edge checks the guarantee of its producer's θ
at the required failure probability, so a build several readers share serves
the readers whose levels its θ meets. Stage 3 only checks the guarantee; it
never resizes. Stage 3's constraint (K) bounds the same estimates with the
same counts (`R` and the fan-in, from one walk,
`asap_accuracy::readout_value_counts_of_queries`, and one test of whether an
input's error reaches the output), so for each query planned alone a
readout at this split meets (K). A level another query gave is checked, not
trusted ("Which estimates a result depends on", below). When two queries
share an execution and need incompatible levels, no level is given for the
pair, and the selection may find no sketch plan for one of them. At equal δ, the scope covering the most values is the
strictest: every group (`G`, when `G` is above 100), then a 1% share (100),
then a 5% share (20), then a single value (1). A whole-result target with an
unbound `G` splits over `R`, so `R` must be an upper bound. A readout
group's values are bounded as SQL optimizers estimate the groups of a `GROUP
BY` (the product of the key columns' distinct counts, clamped to the input
rows), but from declared facts only: by the smaller of the product, over the
scans its keys come from, of the distinct combinations each scan's source
declares for them (at most its series for a PromQL source; 1 without keys),
and the rows its input reads per evaluation (each source's `series_count`
times the samples its window holds at its `sample_interval`, or its
`rows_held`), since there are never more groups than input rows. A key with
no declared count bounds nothing by itself: a SQL measure key (`GROUP BY
bytes`), a computed key or a time. The groups of rows a SQL table declares
do not say which columns form them, so they bound no key; `GROUP BY bytes`
over the last 15 minutes of 6 groups of rows sampled every minute reads at
most 6 x 16 = 96 rows, so `R` = 96, and without a sampling interval or
`rows_held` `R` is unbound. Keys from different scans multiply, since a join
pairs their values: `flows JOIN hosts ... GROUP BY f.src, h.region` with 6
`src` and 6 `region` values declared returns up to 36 values, not 6 + 6.
The "one" side of a many-to-one vector match is another scan in the same
way: `m{side="l"} * on(x) group_left(instance) m{side="r"}` by `job,
instance` pairs one series' `job` with another's `instance`, up to 10 x 10
with 10 (`job`, `instance`) pairs declared. A column of an outer join's
nullable side gains a NULL its source may not hold (`flows LEFT JOIN hosts
... GROUP BY h.region` returns the 6 regions and a NULL), so it counts as
computed and only the rows read bound it. With both unbound the target
stays unmet. A grouped result's `G` follows
the same rule as `R` (`Memo::max_values_per_evaluation` of the root group):
the product, over the scans its group keys come from, of the distinct values
each scan's source declares for them (`SourceFacts.distinct_values_per_label`,
and `distinct_values_per_label_set` for labels that depend on each other: the
smallest product of declared label and label-set counts covering the keys),
clamped to the rows its grouping reads; a PromQL instant vector is also at
most the series read. With each of 625 endpoints in one of 25 services and
`(service, endpoint)` declared at 625, `by (service, endpoint)` returns at
most 625 values, not 25 x 625. `GROUP BY dst` over a table declaring 6
groups of rows and 300 `dst` values returns up to 300 values, not 6. A key
counts only when it is a source column passed through unchanged; a relabeled
or computed key, or an undeclared one, leaves only the rows read to bound
`G`. So a sketch meets a whole-result target over SQL groups when every
grouping column's distinct values, or the rows read, are declared, and over
25 services and 25 endpoints requires each group's sketch to meet
`δ / 625`. Nothing declared leaves `G` unbound; no default stands in for it.

An `AccuracyGuarantee` contains an error metric, an error bound and a
failure probability (possibly unknown, as HLL's). The metric is essential:

| Metric | Quantity bounded |
|---|---|
| Absolute value | Distance from the true value, in its units |
| Relative value | Distance normalized by the magnitude of the true value |
| Rank | Quantile rank displacement normalized by population size |
| Cardinality | Relative error of a distinct count |
| Frequency | Point-frequency error normalized by the stream's L1 norm |
| L2 frequency | Point-frequency error normalized by the stream's L2 norm |
| TopK membership | Whether the selected key set equals the true TopK set |

For example, a small quantile rank error does not imply a small error in the
quantile's numeric value. Converting metrics requires an explicit justified
rule and any evidence that rule needs.

Bounds and probabilities can remain symbolic. An unknown population size,
normalization factor or failure probability stays unknown; it is not replaced
by zero. Provenance records that a value is exact, or the sketch evaluation with its
estimator parameters and contract.
This makes an accuracy decision explainable without reconstructing it from
query text or cost estimates.

## Planning flow

```mermaid
flowchart TD
    Request[Query semantics and accuracy target] --> Generate[Generate candidates with listed parameters]
    Evidence[Scoped source contracts and evidence] --> Generate
    Generate --> Local[Derive local evaluation guarantees]
    Evidence --> Local
    Local --> Compose[Propagate required errors through the DAG]
    Evidence --> Compose
    Compose --> Check[Check the composed target]
    Check -->|Satisfied with sufficient evidence| Eligible[Eligible for cost comparison]
    Check -->|Evidence missing| Pending[Retain an uncertified candidate for explanation]
    Check -->|Known violation or unsupported propagation| Reject[Reject with a reason]
    Eligible --> Select[Select compatible candidates across the workload]
    Select --> Bind[Bind implementations and deployment resources]
```

Candidate generation, certification, selection and deployment binding are
separate decisions. A candidate may remain in the plan space while awaiting
evidence. Current selection excludes candidates marked as missing accuracy
evidence; being present in that space does not authorize execution.

Planner can use an optimistic lower bound while enumerating candidates to
avoid discarding a potentially useful plan solely because evidence is missing.
That bound is a feasibility test, not the candidate's certificate. The original
unknowns remain in its guarantee. Known violations and unsupported propagation
rules are not repaired by optimistic enumeration.

The default target comparator checks only the dimensions requested by the
target; an epsilon-only comparison does not itself check delta. The separate
missing-evidence gate still matters during selection. A successful accuracy
check also does not prove that a runtime implements the plan, that stored state
is ready, or that a complete deployment cost is available.

## Local estimator models and parameter lists

A local model describes a specific evaluation of a specific estimator with
committed parameters and applicable assumptions. A family name or a parameter
such as HLL precision is not, by itself, a confidence certificate.

Planner does not size parameters for a target. Each algorithm has a listed
set of parameters θ; Planner derives the guarantee of each listed θ at the
required failure probability and checks it against the requirement.
Implementation limits or conservative probability bounds can make a listed θ
insufficient for a requirement; that θ then serves no reader that needs more.

The built-in models currently include:

| Model | Current accuracy contract and limits |
|---|---|
| Exact computation | Exact over exact inputs under the supported operation's semantics; approximate inputs still require error propagation |
| KLL | Normalized rank error from the empirical 99% calibration `2.296 / k^0.9723`, fitted on Apache DataSketches' C++ KLL (`a9b42755072b`); below a failure probability of 0.01 that error is scaled by the Gaussian quantile ratio `z(1 − δ/2) / z(1 − 0.01/2)`, a model rather than a measurement, and the guarantee names that contract apart |
| DDSketch | Relative value error from alpha under the supported estimator/domain contract; a quantile's bound holds only over values of one sign (the state column's `column_signs` non-negative or non-positive, "Sign conditions"), else its failure probability is unknown and the readout serves no query with an accuracy target |
| Classic HLL | Relative error certified at the required failure probability over a source declaring at most `max_distinct_per_evaluation` distinct items (summed over the sources scanned below the state), from the linear-counting collision bound under independent uniform bucket hashes; without such a bound, or above the contract's range (`m/2` items at precision `p`), only the RSE magnitude with unknown failure probability |
| CMS | L1-normalized frequency bound from width and depth; does not by itself certify TopK membership |
| CountSketch | L2-normalized frequency bound and median concentration bound, requiring valid odd depth |
| KMV | Provable distinct-count bound from the `k` smallest hashes |
| UnivMon | Exact unit-update total for the supported evaluation; no universal guarantee for all its statistics |
| Other families/evaluations | No default certificate where no accuracy model is registered |

This table describes Planner's registered contracts, not independent
mathematical verification of every estimator or permission to substitute
another implementation with the same algorithm name. In particular, a named
empirical calibration is different from an arbitrary benchmark's maximum
observed error; both its confidence and applicability must remain explicit.

The θ lists are each algorithm's predefined parameter list, `parameter_list`
(`crates/types/src/summary_type/parameter_lists.rs`), weakest first. Pass 1
and Pass 2 add one build per listed θ, and the logical-edge check keeps an
edge only when the guarantee of the producer's θ, at the readout's required
failure probability, is within the required error. Where an error bound grows
as the failure probability shrinks (a KLL's `k`), the same θ meets a reader
at 1e-2 and misses one at 1e-6; no θ is resized for a reader. Accuracy
validation is independent of cost: the guarantee is derived from the
committed parameters, and the cost model never grants accuracy authority.

## Propagating errors through a DAG

The accuracy crate supplies theoretical guarantees (`theoretical_guarantee`),
each operator's error contract (`ErrorContract`, gathered per memo state in
`ErrorMetricsOfMemo`) and the comparison of a bound with a target
(`within_limit`); the deployment's measurements
(`AccuracyEvidence::measured_guarantee`) take precedence over a theoretical
guarantee. Only a sketch readout introduces error. Every other operator
passes its inputs' errors on through a contract, and the contracts are
conservative: an operator with no contract for a metric fails the edge of an
input whose error is in that metric (`RootPruneReason::MetricRule`), so an
approximate value is never treated as exact.

A contract is a linear bound: the consumer's output error is at most
`Σ β_i · ε_i` over its inputs' errors `ε_i`, valid while each input stays
within its validity cap `b_i` (`ErrorContract::error_factor_per_input` and
`linear_bound_holds_up_to_per_input`). The principal contracts are:

| Operator | Contract |
|---|---|
| Sort, TimeRange, TimeShift, Concat, a readout, a finalize, a summary merge | `β = 1` in any metric |
| Filter on columns copied from a source (labels and keys) | `β = 1` in any metric; a filter comparing a computed value has none |
| PromQL `and`, `unless`, `or`; `label_replace`, `label_join` of labels | `β = 1` in any metric on each input whose values are copied; `β = 0` on the right input of `and` and `unless`, whose values are not read. This needs each side's series to be exact: no operator with a contract drops a row by its value, and a readout emits the series the exact evaluation does. A relabel reading a computed value has none |
| Project | Judged on the items reading a computed value (labels, the time and the series identity are copied): `β = 1` in any metric for a copied column; absolute error, `β = 1`, for `abs`, `clamp`, `clamp_min`, `clamp_max` (also PromQL's `promql_` functions) and a negation; relative error, `β = 1`, for `abs` and a negation (`\|−x̂ − (−x)\| = \|x̂ − x\|`, `\|\|x̂\| − \|x\|\| ≤ \|x̂ − x\|`, and `\|−x\| = \|\|x\|\| = \|x\|`) and for a product with or quotient by a nonzero constant |
| Aggregate or exact-accumulator build of counts, min, max, avg, sum | Counts `β = 0`; min and max absolute or relative with `β = 1`; avg absolute with `β = 1`; sum and avg relative with `β = 1` when the values they aggregate are of one sign (the input column's `column_signs` is non-negative or non-positive): `\|Σ (x̂_i − x_i)\| ≤ ε Σ \|x_i\| = ε \|Σ x_i\|`; only when every group-by key is copied from a source (a `by` key as listed; for `without`, every column but the time, the sample value and the excluded ones); none with measure filters or `HAVING`, or with a key reading an estimate |
| `+`, `-` | Absolute error, `β = 1` |
| `/` | Relative error, `β = 1/(1 − b)` on both operands, the denominator capped at `b` |
| `*` | Relative error, `β = (1 + b, 1)`, the right operand capped at `b` |
| Sum in absolute error or of values of unknown sign, comparison, ranking, counter rate or increase, any other function | No contract: the edge of an approximate input fails |
| Top-k over a sketch | Checked: the heap size is at least `k`, and the readout's value guarantee meets the requirement; membership has no check of its own |

`b` is `RELATIVE_OPERAND_VALIDITY_CAP`, 0.1, the weakest error of every
algorithm's list, so it never excludes a listed θ. For relative errors `x`
and `y` with `y ≤ b`, `(1 + x)(1 + y) − 1 ≤ (1 + b)·x + y`, and a ratio of
relative errors `a` over `b` is within `(a + b)/(1 − b)`. A relative bound
below 1 keeps zero at zero and keeps the sign, so a zero denominator gives the
same non-finite answer exactly and approximately, except the sign of a zero:
an exact −0.0 may be read as +0.0, which flips the infinity's sign. The ratio
rule relies on each operand's own relative guarantee (for DDSketch, its
estimator contract: one-signed values within the mapping's range). A sum or
difference of
relative errors has no relative bound (cancellation), and a rank error (KLL)
has no value bound, so neither passes through division. Likewise, an exact
sum above approximate values retains their uncertainty: exact arithmetic does
not recover information lost below it.

Failure probability is not a contract factor: an operator does not scale
it. What fails is an estimate, one value a sketch readout returns in one
evaluation (one series' quantile, one group's distinct count, one key's
frequency), which misses its error bound with probability at most its
readout's δ. A query's result fails only when an estimate it depends on
does, so its failure probability is at most the sum of δ over the estimates
it can depend on (the union bound, without assuming independence between
estimates). Pass 2 splits a query's δ over those estimates; selection checks
the sum for the readouts actually selected ("Which estimates a result
depends on"). A composed query (a ratio of two quantiles) reads each readout
at a level of a failure-probability grid and the selection combines them.
Estimator-local assumptions, such as independent bucket hashing, must be
stated separately. Shared DAG nodes do not create independent errors merely
because several consumers reference them. Sharing never earns a larger δ for
an estimate: an estimate several output values read is one event, which is
why it is counted once per path, not once per reader. Per-result guarantees
also do not imply a simultaneous guarantee across every query or evaluation
time in a dashboard.

### Sign conditions

Two rules hold only over values of one sign. Both read the group property
`column_signs` (declared per source column in `SourceFacts::column_signs`, a
counter's value non-negative, and derived through each operator: a negation
flips it, `abs` makes it non-negative) and test it with
`ValueSign::is_one_sign`, non-negative or non-positive:

- DDSketch's relative bound on a quantile. The state column's sign
  (`GroupProperties::state_column_sign`, the sign of the values the states
  were updated with) must be one sign; otherwise the guarantee
  (`guarantee/ddsketch.rs`) keeps its metric, relative error, with an unknown
  failure probability, so the readout serves no query with an accuracy target
  (`RootPruneReason::NoGuarantee`) and `theoretical_guarantee` returns none.
  `quantile_over_time(0.99, latency[1h])` with `latency` declared
  non-negative holds at α; over a gauge of unknown sign it does not.
- A sum's or an average's relative error (the contracts table): the input
  column's sign must be one sign, as `|Σ (x̂_i − x_i)| ≤ ε Σ |x_i| =
  ε |Σ x_i|` holds then, while values of unknown sign may cancel.

The cost model reads whether a DDSketch state's values are non-negative from
the sources' declared signs instead, so it can disagree with this rule where
an operator changes the sign (DDSketch over `-latency`).

## Readout requirements for composed queries

Pass 2 gives each readout the accuracy it is required to meet. A query's
target error is divided by the factors `β` of the consumers on the path from
the readout to the query root, and capped by the validity caps on the way: the
largest error the readout's output may have for that query.

- A query whose path reads one readout gives one requirement level: the
  target divided by the path's factor, at the target's failure probability
  per value: δ split over `min(R, V · fan_in)`, the readout values the
  output values the scope covers depend on (`R` summed over every readout
  group below it, `Memo::max_values_per_evaluation` each: the product of the
  distinct values its keys' scans declare for them, at most the series for
  PromQL labels and one without keys, clamped to the rows its input reads;
  unbound when neither is known), since a reduction such as `max` fails when
  any readout it reads does. `SELECT count(DISTINCT dst) FROM flows` at δ =
  0.01 reads one distinct count, so its readout keeps 0.01. `max by (job) (q
  or r)` with 3 series per side reads 6 readout values, and one job's value
  may depend on all, so each gets δ / 6; a bare `q or r` copies each output
  value from one readout value, so per value it keeps δ and over a whole
  result of `n_q` values gets δ / `n_q`.
- A composed query (several readouts whose errors reach its output, `β > 0`,
  such as a ratio; `q and r` reads one, since `r`'s values are not read, and
  `q or r` combines none, since each output value is copied from one side
  and its error is that side's) gives every level of the error grid (10%,
  5%, 2%, 1%, 0.5%, 0.2%, 0.1%) and the failure-probability grid (powers of
  ten from 1e-2 down to 1e-5, continued down to the first one at or below a
  tenth of the query's per-value failure probability) not looser than the
  cap. The logical-edge check then keeps the levels a producer's θ meets.
- A readout reached by several queries gets the union of their levels,
  less each level the listed builds that meet it are exactly those that
  meet a stricter one (a copy at it would accept the same producers).
- A count reads no value (`β = 0`), and neither does the right input of
  `and` and `unless`: the readouts below it get no level through it, and
  an operator above it combines no error of them (`q / count(r)` reads
  one readout's error).
- A query whose target sets a failure probability with no per-value one
  (`n_q` or the values per evaluation of a readout below it unknown) gives
  no level. A
  readout no query gives a level is pruned
  (`RootPruneReason::ValuesPerEvaluationUnknown`); one another query gives a
  level stays, and the query reads its group only exactly.

The metric rule cuts a readout's edge when no query reaching it accepts its
metric, a union over the queries. Per query, Pass 2 also lists the groups it
may read only exactly
(`LogicalASAPMemoCandidates::exact_only_groups_of_query`, from
`ErrorMetricsOfMemo::exact_only_groups_of_query`), walking the live
candidates: the groups it reaches through a consumer without a contract for
its target's metric, and the readout groups whose error metric its target
does not allow. With Q0 = `p99 / p50` and Q1 = `p99 / p50 > 1` sharing the
ratio, Q0 may read the DDSketch readouts while Q1 reads the ratio and every
group below it only exactly; with `quantile_over_time(0.9, m[5m])` asked once
at a rank target and once at a relative one, each reads the other's readout
groups only exactly. Pass 2 only computes the list; Stage 3 enforces it per
query, in the query's own selection context (the selection MILP's
constraint (X)): no execution on the query's selected path, from its output
down, in a group on its list is a candidate with sketch parameters, while
another query may still read that candidate through its own path. A scalar
root's operands are roots: `scalar(v)` and a scalar subquery keep the
target's metrics, and any other scalar expression reads its operands only
exactly.

The list is per group, so it is conservative where one group holds
candidates a query may and may not read: `avg by (job)(count_over_time(m[1m]))`
at an absolute target reads `avg` with a contract and the exact accumulator's
sum and count without one, so the group below is exact-only for that query
even through `avg`. A finer list needs Stage 3 to enforce the metric rule per
query on each edge.

A PromQL subquery (`<expr>[10m:30s]`) has no error contract, so every sketch
readout below one serves no query with a target (sound). A contract for it
must split the failure probability over the subquery's inner steps (series ×
range / resolution), not over the series alone.

An exact target gets no approximate level. A model with only a fixed 1%
failure contract cannot meet a level that needs a smaller failure probability;
another supported configuration or candidate is needed.

## Checking each query's target in selection: constraint (K)

Pass 2 gives each readout requirement levels, and the logical-edge check keeps
the producers that meet them. A composed query reads several readouts, each
at one of its levels, and only the selection decides which: Stage 3's
selection MILP checks that the readouts a selection combines compose within
each query's target. This is constraint (K).

**Variables.** Per execution `c` (a candidate in one evaluation context)
that a query with a target reaches, and that is its own original (not in a
copy of an equivalent context), all continuous:
- its error bound `E_{c,m}` per metric `m`;
- `P_c`, the failure probability of one of its output values: the largest,
  over its output values, of the summed δ of the estimates that value
  depends on;
- `W_c`, the failure probability of its whole output: the summed δ of every
  estimate any of its output values depends on. It exists only where every
  readout below `c` bounds its estimates per evaluation (`R` known).

Each exists only where some selection makes it positive and its error is
not constant. Per input `i`, `F_{c,i,m}`, `HP_{c,i}` and `HW_{c,i}` are what
`c` reads from the producer selected on `i`. `P` and `W` exist only where a
query root above checks them, through the rows that read them: a root
checking each covered value reads `P`, one checking every estimate read
`W`; `P` reads `P` of an input read value by value and `W` of one read
whole; `W` reads `W` (`ErrorModel::checked_failure_probabilities`). With
both created wherever bounded, RangeHeavy-8users solved about 3× slower
with the same plan.

**Rows.** With `y_{u,c,i}` the edge variables and `z_{q,v}` the choice of
execution `v` answering query `q`:

- `E_{c,m} ≥ ε_c[m = m_c] + Σ_i β_{c,i,m} F_{c,i,m}`, with
  `F_{c,i,m} ≥ E_{u,m} − M_{u,m}(1 − y_{u,c,i})` per selectable producer `u`;
  `ε_c` is a readout's own error, its required accuracy
  (`CandidateForMemoGroup::required_accuracy`), 0 for any other candidate.
  An operator that copies each output value from one input (`or`, a
  concatenation) has one row per input instead of the sum: its output's
  error is one input's, `E_{c,m} ≥ ε_c[m = m_c] + β_{c,i,m} F_{c,i,m}` for
  each `i`.
- The metric rule on the selected path: `E_{u,m} ≤ M_{u,m}(1 − y_{u,c,i})`
  when `c` has no contract in `m`. An input with `β = 0` passes none.
- The validity cap: `F_{c,i,m} ≤ b_{c,i}` where the contract's linear bound
  holds only up to `b` (a ratio's denominator).
- Failure probabilities, with `HP_{c,i} ≥ P_u − M^P_u(1 − y_{u,c,i})` and
  `HW_{c,i} ≥ W_u − M^W_u(1 − y_{u,c,i})` per selectable producer `u`, over
  only the inputs whose estimates reach the output (`β > 0` in a metric of
  `c`'s contract, or values chosen by estimates, the test Pass 2's split
  uses; a count's input and the right input of `and` and `unless` add
  nothing unless a top-k chooses their values):
  - a readout: `P_c = δ_c` and `W_c = R_c δ_c`, with `δ_c` its required
    failure probability and `R_c` its estimates per evaluation
    (`Memo::max_estimates_per_evaluation`); a top-k readout, whose output
    keys are chosen by every estimate it ranks, has `P_c = W_c = R_c δ_c`.
    Constants, as a readout's error is;
  - any other execution:
    `P_c ≥ Σ_{i read value by value} HP_{c,i} + Σ_{i read whole} HW_{c,i}`,
    where an operator each of whose output values depends on one input's
    estimates (`or`, a concatenation, with no top-k choosing an input's
    values) has instead `P_c ≥ HP_{c,i} + Σ_{i read whole} HW_{c,i}` for
    each input `i` read value by value; and `W_c ≥ Σ_i HW_{c,i}`. The
    operator classes are in the table below.
- At the outputs of a query `q` with a target: `E_{v,m} ≤ ε_q + M(1 − z_{q,v})`
  in each metric the root allows (the target's at an operator root, none for
  the operands of a scalar expression no contract covers, 0 elsewhere). With
  a δ, one failure row per query, over every output `v` it may be answered
  from, by the bound `AccuracyTarget::failure_probability_bound` names from
  `V` (the values the target's scope covers) and the query's counts `R` and
  fan-in (Pass 2's `ReadoutValueCounts`). Pass 2 records the counts it split
  δ by (`LogicalASAPMemoCandidates::readout_values_of_query`), and (K)
  checks the bound they name:
  - every estimate read, when `R ≤ V · fan_in` or `V` is unbound:
    `W_v ≤ δ_q + M(1 − z_{q,v})`;
  - each covered value, when `V · fan_in < R` or `R` is unbound:
    `P_v ≤ δ_q / V + M(1 − z_{q,v})`;
  - neither, when both are unbound: no row. Pass 2 gave the query no level,
    and constraint (X) makes it read the readout groups below it only
    exactly; that, not (K), is what keeps it from reading a level another
    query gave.

  An output whose checked term is unbounded under some selection (a readout
  below with unknown `R`) is not answered by that execution. A query with
  several roots (a scalar expression over them) sums its roots' terms in its
  one row. Each limit is widened as the edge check widens it
  (`tolerated_limit`).

**Which estimates a result depends on.** The union bound is taken over
estimates, not over readouts or output values.

*Soundness.* Every operator above a readout is deterministic, and its error
contract holds whenever its inputs are within their bounds. So an output
value misses `ε_q` only if an estimate it depends on misses its bound. Which
estimates an output value depends on is decided by the data's series and
keys (labels, group keys, join keys), not by the estimates. The exceptions
are operators that choose by estimated value: a top-k readout is charged for
every value it ranks, and any other such operator (a comparison, a `LIMIT`
after ordering by an estimate) has no contract. An operator keeping or
taking values by the keys a top-k chose (`p and topk(3, q)`, `p unless
topk(3, q)`, `topk(3, q) or p`, also through operators reading the top-k
value by value) depends on every estimate the top-k ranks: that input's
estimates count even where `β = 0`, and `or` sums its sides' `P`
(`asap_accuracy::values_chosen_by_estimate`). So for any data `D` the
statement fails only on `⋃_{e ∈ S_q(D)} E_e`, where `S_q(D)` is the
estimates the covered output values depend on and `E_e` is estimate `e`'s
miss. Hence

    P(fail) ≤ max_D Σ_{e ∈ S_q(D)} δ_e.

Both root rows bound the right side for every `D` the declared facts allow:
- every estimate read: `S_q(D)` lies in the estimates of the readouts below,
  at most `R_c` per readout per path, so the sum is at most `W_v`;
- each covered value: `S_q(D) = ⋃_{v covered} deps(v)`, at most `V` values,
  each with `Σ_{e ∈ deps(v)} δ_e ≤ P_v`, so the sum is at most `V · P_v`.

The recursions are sound step by step:
- a value read value by value is one input value, whose estimates sum to at
  most `P_u`;
- a value copied from one input (`or`) is that input's value, so the largest
  of the inputs' `P` bounds it;
- an input read whole (a reduction) may contribute every estimate of that
  input, at most `W_u`;
- a whole output depends on at most the union of its inputs' estimates, at
  most `Σ W_u`.

An exact input has δ = 0. An input with `β = 0` contributes only which series
or keys it has. A readout returns exactly the series the exact evaluation
does, so those estimates cannot make the output miss, except a top-k's,
whose keys are chosen by its estimates (above).

*The assumption it rests on.* Soundness needs `R_c` to be an upper bound on
the estimates one evaluation of readout `c` computes, under every dataset
the declared facts allow; it is the only declared count the bound trusts
(`n_q` only decides which row is checked). `R_c` is
`Memo::max_estimates_per_evaluation`: for a readout other than a top-k its
values, `Memo::max_values_per_evaluation`, the product of the distinct
values its state's keys' scans declare, at most the series for PromQL
labels, clamped to the rows its input reads. The declared distinct items
are the deployment's contract, as HLL's are: declaring fewer than a state
counts gives a too small `R_c`. Two cases checked against it:
- A top-k readout ranks every distinct item of each state, more than the
  values it returns. Its `R_c` is the smaller of its states' count times the
  declared distinct items per state (`SourceFacts::max_distinct_per_evaluation`)
  and the series or rows its items come from, both upper bounds on the items
  (`Memo::max_estimates_per_evaluation`).
- A time-bucketed SQL grouping (`GROUP BY src, date_trunc('minute', ts)`)
  returns buckets × keys values. A time or computed key bounds nothing by
  itself, so `R_c` falls back to the rows the grouping reads, which bounds
  the groups whatever the buckets. The executed bounds check
  (`memo_value_bounds`) runs these shapes on data the facts allow.

*Scopes.* `PerValue` covers one value: `V = 1`, so the row is `P_v ≤ δ`, with
no division by `n_q`. `WholeResult` covers `V = n_q`.
`GroupsWithRecordShareAtLeast(s)` covers `V = min(⌊1/s⌋, G)` groups. Which
groups those are is decided by exact record counts, not by estimates, so the
same two rows hold with that `V`, and `W_v` (all groups) stays a sound
alternative.

*Operator classes* (shared with Pass 2's split:
`asap_accuracy::inputs_read_value_by_value`,
`ErrorMetricsOfMemo::each_value_depends_on_one_input` and the "estimates
reach the output" test `ErrorMetricsOfMemo::input_estimates_reach_output`):

| Class | Operators | `P_c` | `W_c` |
|---|---|---|---|
| Readout | `SummaryEstimate` (not top-k) | `δ_c` | `R_c δ_c` |
| Readout choosing by estimate | top-k | `R_c δ_c` | `R_c δ_c` |
| Value by value | filter on copied columns, project, sort, time shift, relabel, label enrichment, series sample; binary arithmetic, one-to-one or many-to-one (`on`, `ignoring`, `group_left`, `group_right`: each output value reads one value of each side, and the "one" side's value is read by many outputs but is one estimate); join | `Σ_i HP` | `Σ_i HW` |
| Each value from one input | `or`, concatenation (`UNION ALL`); `and`, `unless` (left only, the right has `β = 0`) | `max_i HP`; `Σ_i HP` when a top-k chooses an input's values | `Σ_i HW` |
| Input read whole | aggregates (`sum`/`max`/`avg` by or without keys), any operator whose output value may read many values of an input, the operand of `scalar` | `Σ_i HW` | `Σ_i HW` |
| No contract | a subquery, a comparison, a counter rate | none: no error, and so no estimate, passes | none |
| `β = 0` input | a count's input, the right of `and` and `unless` | adds 0, or as read value by value or whole when a top-k chooses its values | adds 0, or `HW` when a top-k chooses its values |

*Same rule as Pass 2, for a query planned alone.* Pass 2 gives each readout
of a query one level `δ / min(R, V · fan_in)`. Its `R` and fan-in add, per
path, the terms `W` and `P` add, and take the largest over a group's
candidates where the rows take the selected one. So at that level,
`W_v ≤ R · δ / min(…)` and `V · P_v ≤ V · fan_in · δ / min(…)`. The row the
root checks is the one whose count is the `min`, so it is at most δ. A level
a stricter rule gave (any `δ_c ≤ δ / max(V, R)`) meets it too. A level given
for another query with a looser split does not: `max(q)` over 1,000 series
cannot read a copy at the 0.01 a per-value query on `q` was given, because
`W = 1,000 × 0.01 > 0.01`.

*What the rows lose* (all sound; each only asks for a stricter δ than
needed):
1. An estimate one query reaches on several paths counts once per path:
   `max(q) − min(q)` over 1,000 series charges `2,000 δ_c`, although 1,000
   estimates fail at most. Pass 2's `R` counts it the same way, so no level
   it gives becomes infeasible.
2. The root checks one of the two rows, chosen from the counts. When the
   readouts below have different δs (a composed query), the other row may
   have been smaller.
3. An operator that returns fewer values than it reads does not lower the
   count of a whole input (a label filter above a shared readout, a join
   dropping unmatched rows), as in Pass 2's `R`.

Counting each estimate exactly once, per query, needs a variable per
(query, execution) on the query's selected path, and at each `or` a dual
variable bounding how many output values each side supplies. That is linear
but grows with queries × executions; it is not used.

**Constant errors.** A readout's state is exact, so its error and failure
probabilities are its own under every selection: it has no `E`, `P` or `W`
variable, and its consumers read `Σ_u ε_u y_{u,c,i}`, `Σ_u δ_u y_{u,c,i}` and
`Σ_u R_u δ_u y_{u,c,i}` (at most one `y` of an input is 1). This is
exactly the variable form with `E_u = ε_u`, and it removes the big-M rows of
every readout.

**Ceilings.** `M` (and `M^P`, `M^W`) is the largest error and failure
probability any selection gives an execution (unbounded for `P` or `W` when
a readout below has unknown `R`; then the execution has no such variable),
computed in one pass in the memo's topological order (every selectable edge
reads a group ordered before the consumer's), each input taking its largest
producer's ceiling, by the same composition rule as the rows. The rule is
written once (`AccuracyRules::compose`, composing `E`, `P` and `W`), and
also checks a whole selection (`AccuracyRules::selection_meets_targets`, the
same root row): the greedy upper bound that prunes
executions dearer than a feasible plan uses its plan only when it meets (K),
since the cheapest readouts of a ratio may each meet their level and compose
above the target.

**Solver tolerance.** HiGHS's feasibility tolerances are absolute, 1e-7 by
default. A row checking every estimate read compares sums of the size of δ
itself (0.01 against 0.01), where 1e-7 is negligible. A row checking each
covered value compares `P_v` with δ / V, often far smaller: δ = 0.01 over a
whole result of 1e4 values is 1e-6 per value, of which 1e-7 is a tenth: two
readouts failing with 5.4e-7 each compose to 1.08e-6 and pass that limit
within the default tolerance. So HiGHS first solves at its defaults, and the
selection is then checked exactly (`AccuracyRules::selection_meets_targets`,
in f64). A selection that misses a target is solved once more at primal and
MIP feasibility tolerance 1e-10, the least HiGHS accepts, and checked again.
A row may still pass its limit by up to 1e-10, which for a per-value δ near
or below 1e-10 can be orders of magnitude: at δ = 1e-12 per value (δ = 0.01
over 1e10 values) the solver may select readouts failing with 1.08e-12 in
all. A selection that misses a target after the second solve is an error
naming the query and its bound with its limit ("an accuracy target is below
the solver's precision"); planning never returns a plan that may not meet its
target. The stricter tolerance is paid for only when the check needs it:
solving every (K) at 1e-10 made RangeHeavy-8users about 6× slower, deriving
the tolerance from the smallest limit solved RollupHeavy-8users about 2.8×
slower, and scaling (K)'s rows by their limits solved RangeHeavy-8users
about 4× slower, and dividing every failure probability by the smallest
root limit (so the tightest limit is 1) solved RangeHeavy-8users 10% and
RangeHeavy-8users-counter 48% slower.

**Context copies.** A copy of an equivalent evaluation context shares its
original's variables and has no rows of its own. Two contexts are copies only
when each output's root limits (the tolerated error per metric, and the
failure probability bound's kind and limit, all the rows read of it) are
equal, so the copy's rows would repeat the original's:
- queries at δ = 0.01 per value returning 10 and 100 values are copies;
- over a whole result they are copies when both check every estimate read
  (a readout of 10 series declaring 10 and 50 values);
- they are not copies when they check each covered value of 10 and of 100
  values.

### Example: allocation in a ratio

A is `quantile_over_time(0.95, latency[1h])` at rank error 0.01, and B is
`quantile_over_time(0.99, latency[1h]) / quantile_over_time(0.5, latency[1h])`
at relative error 0.03, over non-negative `latency`. A reads a KLL readout at
rank 0.01. B's ratio passes each operand's relative error on with factor
`1/(1 − 0.1) = 1.111`, so Pass 2 gives each DDSketch readout below it the
levels of the grid up to its cap; with levels 0.02 (cost 1) and 0.01 (cost 3):

| p99 readout | p50 readout | B's composed error | meets 0.03 |
|---|---|---|---|
| 0.02 | 0.02 | 1.111 · 0.04 = 0.044 | no |
| 0.02 | 0.01 | 1.111 · 0.03 = 0.033 | no |
| 0.01 | 0.01 | 1.111 · 0.02 = 0.022 | yes |

(K) selects both 0.01 readouts, cost 6. A greedy plan reading the two
cheapest readouts (cost 2) misses B's target, so it bounds nothing.

### Example: failure probability in `or`, many-to-one matches and reductions

All at δ = 0.01 over the whole result unless stated.

| Query | Facts | Pass 2's level (`δ / min(R, V · fan_in)`) | Row (K) checks | Value | Estimates that can fail at once |
|---|---|---|---|---|---|
| `q or r` | 100 series each, same labels, `n_q` = 100 | `R` = 200, fan-in 1: `0.01 / min(200, 100) = 1e-4` | each covered value: `max(1e-4, 1e-4) ≤ 0.01 / 100` | 1e-4 ✓ | 100 (each output copies one) |
| `q * on() group_left e` | `q` exact over 1,000 series, `e` one sketched series, `n_q` = 1,000 | `R` = 1, fan-in 1: `0.01 / min(1, 1,000) = 0.01` | every estimate read: `W = 0 + 1 · 0.01` | 0.01 ✓ | 1 (every output reads `e`'s one estimate) |
| `max(q)` | 1,000 series, `n_q` = 1 | `R` = 1,000, fan-in 1,000: `1e-5` | every estimate read: `1,000 · 1e-5` | 0.01 ✓ | 1,000 |
| `count(distinct user_id)` by service, per value | δ = 0.05, ε = 0.05 relative, 10,000 services | `R` = 10,000, `V` = 1, fan-in 1: `0.05 / 1 = 0.05` | each covered value, `V` = 1: `P = 0.05` | 0.05 ✓ | 1 per value |
| `A or B`, `A` = `quantile_over_time(0.99, m{env="prod"}[5m])`, `B` = `quantile_over_time(0.99, m[5m])`, both reading one readout of `m` | 100 series, `n_q` = 100 | `R` = 200 (one readout, two paths), fan-in 1: `1e-4` | each covered value: `max(1e-4, 1e-4) ≤ 0.01 / 100` | 1e-4 ✓ | 100 |
| `max by (job) (A or B)` | as above, 10 jobs | `R` = 200, fan-in 200: `5e-5` | every estimate read: `W = 200 · 5e-5` | 0.01 ✓ | 100: the rows charge each estimate twice (once per path), so 1e-4 would have sufficed |

Charging the selected readout's δ once against δ / `n_q` would instead:
- reject `q or r` (`2e-4 > 1e-4`) and the many-to-one match
  (`0.01 > 0.01 / 1,000`), leaving them no sketch plan;
- accept for `max(q)` any copy of the readout, including one a per-value
  query on `q` was given at 0.01, whose 1,000 estimates may fail with up to
  1,000 × 0.01.

For the distinct count, a split over `max(V, R)` would ask each group's KMV
for 0.05 / 10,000 = 5e-6, k = 26,656. The per-value row asks only that each
group's own estimate fail with at most 0.05, which KMV meets at k = 4,323.

## Evidence and trust boundaries

`AccuracyEvidence` supplies the deployment's measured guarantees. The evidence must cover the population to which the
claimed guarantee applies: sources, filters, grouping, evaluation windows and
all merged tumbling windows. Evidence for a narrower population cannot silently certify
a wider one. Data-workload facts are declared: the planner has no clock, so the caller
drops facts that no longer hold before planning.

Source contracts are assertions that the source or deployment must establish
and enforce. They are not inferred from observed cardinality or sampled value
ranges. A source-specific deployment should supply those facts, rather than
reimplement estimator mathematics or candidate legality.

ERP benchmark results can inform resource costs and measured behavior.
A measured maximum error does not establish a failure probability. Benchmark
preference for cost estimation must therefore remain separate from accuracy
certification. If a model uses an empirical accuracy calibration, the contract
must identify its confidence level and scope rather than silently promoting
an observation into a guarantee.

### Example: HLL distinct counts

HLL's generic guarantee is its relative standard error, `1.04 / √m` for `m`
buckets, with an unknown failure probability. Its readout's metric is relative
error, but no failure probability can be evaluated, so without more a distinct
count read from HLL serves no query with an accuracy target
(`RootPruneReason::NoGuarantee`). When the data workload bounds the distinct
items one evaluation counts (`SourceFacts::max_distinct_per_evaluation`,
summed over the sources scanned below the state), classic HLL's
linear-counting branch is certified: `COUNT(DISTINCT user_id)` within relative
0.05 at δ = 0.01 holds at precision 18 over at most 9,636 distinct users, and
over 1,000,000 (above the contract's `m/2`) it does not. KMV's distinct count,
whose bound does not depend on the number of distinct items, serves such a
query without a bound. Missing evidence leaves generic HLL confidence unknown.
Invalid or infeasible contracts cannot authorize the result.

## Sharing across consumers

Accuracy reconciliation considers semantically compatible consumers with
different accuracy requirements. A looser consumer may reuse a tighter
consumer's computation when the supported reconciliation rule proves that the
requirements, grouping and row identity permit it.

This creates an explicit reuse candidate and a dependency on the tighter
computation. It does not change ordinary common-subexpression equality, merge
queries with different semantics, or weaken the tighter consumer's target.
Global selection still coordinates compatible choices and accounts for shared
cost. The current reconciliation strategy does not reconcile exact and
approximate requirements merely by ordering their epsilon values.

### Example: two consumers of the same grouped quantile

Suppose two queries compute the 95th-percentile latency from the same source,
filter, time window and `service` grouping. They have identical output columns
and differ only in accuracy requirements:

| Consumer | Requested result | Rank error target | Failure-probability target |
|---|---|---|---|
| A | p95 latency per service | 1% | 1% |
| B | p95 latency per service | 5% | 1% |

Planner can propose building the tighter KLL computation required by A and
letting B read that same result. A result certified to 1% rank error also meets
B's 5% rank-error target, at the same confidence. The `service` grouping
provides the stable row identity required by the current reconciliation rule.
These percentages bound rank displacement, not latency-value error.

```mermaid
flowchart LR
    Input[Same source, filter and window] --> Shared[KLL computation per service meeting A]
    Shared --> A[Consumer A: 1% rank error]
    Shared --> B[Consumer B: 5% rank error]
```

B's own candidate, a θ meeting only B, remains available. Global selection compares
that candidate with the reuse candidate and preserves the dependency on A's
tighter computation if it chooses reuse. Sharing is therefore an explicit
planning choice, not an unconditional rewrite or two separately charged builds.

This reconciliation does not apply if B instead asks for p99, uses a different
window or filter, or groups by another key: those queries differ in semantics,
not just accuracy. Nor does this example justify serving B at a failure target
of 0.1%; A's 1% failure guarantee would not meet it. A separate supported reuse
rule or a different estimator configuration would be needed in those cases.

When the consumers' failure targets differ, each reads the shared KLL at
its own required failure probability: a θ whose guarantee meets both
requirements serves both, and one meeting only the looser does not serve
the tighter consumer. No θ is resized for sharing.

## Organization and extension contract

The `asap_accuracy` crate separates these responsibilities:

```text
accuracy/src/
├── lib.rs              # Re-exports: the public interfaces
├── evidence.rs         # AccuracyEvidence: the deployment's measured accuracy
├── error_contract/     # ErrorContract: each operator's error contract per
│                       # metric
├── metric_rule/        # ErrorMetricsOfMemo: the contracts, allowed metrics and
│                       # readout metrics of one memo state; the metric rule per
│                       # query (exact_only_groups_of_query)
├── readout/            # readout_error_metric: the metric of a readout's error
├── readout_caps/       # readout_levels → ReadoutRequiredAccuracy: caps through
│                       # the error contracts, each readout's required-accuracy
│                       # levels; MAX_ERROR_LEVELS
├── logical_edge/       # ErrorMetricsOfMemo::edge_meets_logical_requirements
│                       # (α^log); within_limit and tolerated_limit, the
│                       # requirement comparison
└── guarantee/
    ├── mod.rs          # theoretical_guarantee (one lookup at a failure
    │                   # probability per algorithm)
    ├── binomial.rs     # binomial_upper_tail
    ├── kll/
    ├── ddsketch.rs     # relative error α; its sign condition
    ├── hll/            # HLL: classic certificate over a distinct bound, else
    │                   # RSE (unknown failure probability)
    ├── cms/
    ├── count_sketch/
    ├── cardinality/    # KMV model
    └── univmon/
```

A directory is a module (`mod.rs`) with its unit tests beside it
(`tests.rs`). Each estimator module owns its local accuracy formulas: the
guarantee of a listed θ at a failure probability. The unified entry point delegates to these models; it does not
contain a second copy of their mathematics. Serializable guarantee and metric
types live in `asap-types` so planning, explanations and downstream binding
share the same contract.

Adding an estimator or an operator's error contract requires:

1. A precisely defined error metric, estimator/evaluation semantics and assumptions.
2. A θ list and a guarantee derived from each listed θ at a required failure probability, including unsupported parameter domains.
3. Explicit evidence requirements, population scope and provenance.
4. Propagation rules where supported; rejection or retained unknowns elsewhere.
5. Tests from target and evidence through the guarantee at the required failure probability, error propagation and selection, including missing evidence and infeasible targets.

Algorithm-specific mathematics belongs behind this common contract. It does
not require a new selection rule for every sketch. A deployment's measured
guarantees (`AccuracyEvidence`) carry the same obligation to justify
metrics, assumptions and propagation.

For implementation details, see the [accuracy crate](../../../crates/accuracy/src/lib.rs),
[guarantee representation](../../../crates/types/src/accuracy/guarantee.rs), and
[accuracy propagation companion](../../develop_docs/end-to-end-accuracy-guarantees.md).
