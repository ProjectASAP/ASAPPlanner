# ASAPPlanner Supporting Different End-to-End Use Cases

We design ASAPPlanner system architecture as in [planner-layering.md](planner-layering.md).
Next, in this doc, we introduce how to make ASAPPlanner support a new use case, from the perspectives of 
* How to define the query workloads, and data workloads, based on the use case/application pattern.
* Based on the application pattern, how to determine the suitable ASAP Primitives to use.
  * If the capabilities of ASAP Primitives are missing in current ASAPPlanner, how to add them.
* How to integrate ASAPPlanner to a deployment runtime for that use case, so the system shows an end-to-end benefits.

## Contents

[TODO: just fill]

- [Given a use case, how to define the query workloads and data workloads](#given-a-use-case-how-to-define-the-query-workloads-and-data-workloads)
- [List of (Examples) Query Patterns Mapping to ASAP Primitives](#list-of-examples-query-patterns-mapping-to-asap-primitives)
- [Process of Adding a New ASAP Primitive to ASAPPlanner](#process-of-adding-a-new-asap-primitive-to-asapplanner)
- [Implementing Your Own Planning Logic in ASAPPlanner](#implementing-your-own-planning-logic-in-asapplanner)
- [Integrating ASAPPlanner to the Use Case Deployment](#integrating-asapplanner-to-the-use-case-deployment)
  - [Workflow of ASAPPlanner and Deployment](#workflow-of-asapplanner-and-deployment)
  - [Key Interfaces between ASAPPlanner and Use Case Deployment](#key-interfaces-between-asapplanner-and-use-case-deployment)

## Given a use case, how to define the query workloads and data workloads

- We need to run the use case and record the traces of the queries and data, and then
 - We can analyze the data shape (data cardinality, data distribution, dimensions, structured data or unstructured data but we can abstract the structured part out of it).
 - We can analyze the pattern of the queries, such as what parts of the queries have overlapping, redundant computation / data scanning, and what is the query intent, mapping to a potential ASAP Primitive family (see the ASAP Glossary).

Key points (to be expanded):

- **Output of this step: a `PlanningWorkload`** = query workload + data workload, the input to the planner ([planner-layering.md](planner-layering.md), stage inputs table). Each input is consumed by one stage.
- **Query workload, per query** (from the query trace):
  - query text and language (PromQL / SQL / MetricsQL), or a translation into one of them;
  - accuracy target (exact, or ε/δ with its error metric: absolute, relative, rank);
  - latency requirement;
  - recurrence (one-time, repeated every *t*, unknown) and predictability (ad hoc vs known in advance), measured as invocation counts and intervals in the trace;
  - time selection (lookback, range, `as_of`).
- **Data workload, per source** (from the data trace):
  - schema (columns, types, time column, which columns are dimensions vs values);
  - arrival (at rest, continuously ingesting, mixed), ingestion rate, retention;
  - cardinality (number of series / distinct keys per dimension) and value distribution (sign, range, skew), which decide sketch validity and sizing;
  - unstructured data: extract the structured part (e.g. labels, fields) as the source schema; the rest stays outside the planner.
- **Overlap analysis across queries** (what makes ASAP worthwhile):
  - identical sub-expressions across queries (shared computation);
  - the same data scanned by several queries with different statistics (one summary answering several);
  - overlapping windows across evaluations (panes, sliding windows);
  - recurring queries over streaming data (state maintained at ingestion).
- **Query intent → ASAP Primitive family**: classify each aggregation by intent (count, quantile, top-k, distinct count, frequency moments, ...) as Stage 1 Pass 1 does; see the next section.
- **Statistics that are not measurable in the trace** are declared as unknown; the planner then stays conservative (e.g. no guarantee where a bound needs them).

## List of (Examples) Query Patterns Mapping to ASAP Primitives

Key points (to be expanded):

| Query pattern (intent) | Example | Candidate ASAP Primitives | Conditions on the workload |
|---|---|---|---|
| Count / sum per group | `sum by (job) (rate(x[5m]))` | exact accumulators | always exact; ASAP benefit comes from sharing and materialization |
| Quantile | `quantile_over_time(0.99, x[5m])`, `approx_percentile_cont` | KLL, DDSketch | DDSketch needs the value range (relative error); KLL sized by ε and δ |
| Top-k / heavy hitters | `topk(10, sum by (k) (...))` | Count-Min + heap, Count Sketch + heap | Count-Min needs non-negative weights (e.g. counters) |
| Distinct count | `count(count by (user) (...))`, `COUNT(DISTINCT u)` | HLL, KMV | a bound on distinct keys for a confidence |
| Frequency moments (L2, entropy) | second moment / entropy of a key distribution | UnivMon (one sketch answers several) | mergeable across panes |
| Many groups (high cardinality) | per-key statistics over millions of keys | Hydra (one state over all groups) vs one sketch per group | Stage 3 picks on cost |
| Overlapping windows | `x[1h]` evaluated every minute | per-pane summaries merged per window | mergeable families only |
| Same sub-expression in several queries | dashboard panels over one metric | one shared node (Pass 2 ASAP-aware CSE) | identical expression, or one summary able to answer all consumers |

- Each row is eligible only when the query's intent matches; valid only when the target can be met under the declared data workload; selected only when it is cheapest for the whole workload.
- Example walkthroughs: [planner-layering.md](planner-layering.md) Examples 1–4.

## Process of Adding a New ASAP Primitive to ASAPPlanner 

Key points (to be expanded; extends [planner-layering.md "Adding a new summary family"](planner-layering.md#adding-a-new-summary-family)):

1. **Declare its capability**: the query intent(s) it answers, input value types, weight domain (e.g. non-negative only), mergeability / subtractability, window support, grouping strategy (per group vs one state over all groups). Schema per [asap-primitive-schema.md](asap-primitive-schema.md).
2. **Stage 1, Pass 1**: offer it (unsized) wherever its intent matches.
3. **Sizing and theoretical error bound**: size from (ε, δ); give the error bound and its metric. Without a theoretical bound, it is valid only through measured evidence.
4. **Stage 2**: its physical operator, and when it can be materialized at ingestion or at query time.
5. **Stage 3**: state bytes and update / merge / estimate cost, so it can be priced.
6. **Execution**: a kernel in the executor wrapping the sketch library (fix gaps in the library itself), with build, merge and estimate semantics and capability checks.
7. **Measurements**: accuracy and cost at canonical shapes (sketch-bench), feeding evidence and measured costs.
8. **Tests**: Pass 1 offer, guarantee, Stage 3 validity and cost, executor result vs exact within the bound, and one end-to-end example through the stage pipeline.

- What does not change: frontends, the stage structure, other families' candidates.

## Implementing Your Own Planning Logic in ASAPPlanner

Key points (to be expanded):

- **Where planning logic plugs in: `PlanningModels`**, passed with the workload in `UserInput` to `e2e_plan` or `plan_installable`. A use case keeps the planner's stages and replaces the decisions it owns, without forking the planner. `PlanningModels::builtin()` gives the defaults.

| What you decide | Interface (in `asap-plan-selection` unless noted) | Default | Read by |
|---|---|---|---|
| Whether a guarantee meets a target | implement `AccuracyModel::satisfies` | `BuiltinAccuracyModel` (the analytical rules) | Stage 3 |
| Accepting an output whatever its guarantee (e.g. your own validation) | override `AccuracyModel::accepts_output_regardless_of_guarantee` | accepts none | Stage 3, checked before the guarantee |
| Facts about your data the error bounds need, and measured accuracy of whole outputs | implement `AccuracyEvidenceProvider` (`operand_domain`, `output_guarantee`) | `NoAccuracyEvidence` | Stage 3 |
| How plans are compared | `PlanSelectionSettings.cost_model`: cost model 1 (pay per use), 2 (provisioned), 3 (your own cluster's limits and an objective) | cost model 1, illustrative prices | Stage 3 |
| Prices and speeds | `ResourceUnitCost` (e.g. the AWS Fargate preset), `ResourceSpeed` | illustrative | Stage 3 |
| Your runtime's measured costs | `PlanSelectionSettings.measured_operation_costs` (`MeasuredOperationCost`) | empty: priced from formulas | Stage 3 |
| What your runtime can run and keep | `DeploymentCapabilities` (`asap-logical-asap-optimization`): supported summaries, raw data retained, query-time results kept | everything supported, raw retained | Stages 1–3 |

- **Example**: a deployment that trusts only exact answers implements `AccuracyModel` with `satisfies` returning `guarantee.is_exact()`, and passes it as `PlanningModels.accuracy`. The candidates stay the same; only the ones Stage 3 accepts change (`crates/planner/tests/deployment_accuracy_model.rs`).
- **Settings as data**: `PlanSelectionSettings` and `DeploymentCapabilities` load from JSON (`stage_pipeline --plan-selection-settings`, `--deployment-capabilities`), so a deployment can change them without code.
- **What is not pluggable** (change it in the planner instead):
  - candidate generation (Stage 1 Pass 1 and 2) and guarantee composition, which use the built-in rules;
  - Stage 2 materialization;
  - new summaries, which follow the previous section.
- **Rules**: models are planning logic only; they never execute. Evidence is a scoped fact about one deployment. A model cannot make a non-executable plan valid: a Count-Min plan over weights not proven non-negative is rejected whatever the model says.

## Integrating ASAPPlanner to the Use Case Deployment

### Workflow of ASAPPlanner and Deployment

Key points (to be expanded):

1. **Collect**: the deployment records query and data traces and builds the `PlanningWorkload` (previous sections), plus its models (cost model and prices, accuracy evidence) and capabilities (supported families, operators, materialization media, budgets).
2. **Plan**: call the planner (Stage 0 → Stage 3). It returns one selected plan, or an error with every rejection reason. The planner never executes and has no execution fallback.
3. **Validate and install**: validate the plan document (schema version), check that the deployment can run every operator, then install it.
4. **Execute (Stage 4)**: the deployment runs the plan as given: ingestion-time state, storage, query-time evaluation.
5. **Observe and re-plan**: observed error, cost and cardinality flow back as evidence and calibration; a change in the workload (query added / removed), expired evidence, cardinality drift or new prices triggers re-planning of the whole workload. The transition between installed plans (keep / rebuild / backfill state) is owned by the deployment.
6. **Show the end-to-end benefit**: compare against the baseline without ASAP (exact plans) on the deployment's cost model and the query results' error.

### Key Interfaces between ASAPPlanner and Use Case Deployment

Key points (to be expanded):

| Direction | Interface | Content |
|---|---|---|
| Deployment → planner | planning entry point (`e2e_plan`) | query workload, data workload, frontend context (catalog, `now_ms`) |
| Deployment → planner | `PlanningModels` | accuracy model and evidence (`AccuracyEvidenceProvider`), cost model / prices (`ResourceCalibration`) |
| Deployment → planner | capabilities (to be defined) | supported summary families and operators, materialization media, memory / disk budgets |
| Planner → deployment | `PhysicalASAPDAGDocument` | the selected plan: operators, summaries and sizes, materialization (at ingestion / at query), schema version |
| Planner → deployment | selection report | per-candidate costs, guarantees, rejection reasons (today exposed by the stage pipeline only) |
| Deployment → planner (feedback) | evidence and calibration | observed error (`output_guarantee`), measured costs, cardinality updates |

- One bundle for all deployment inputs is still to be defined.
- The plan document is versioned; the deployment refuses a mismatched version.


