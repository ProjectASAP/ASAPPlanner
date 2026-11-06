# Memo groups, properties and requirements

This document explains how the optimizer represents alternatives, checks whether they can be composed, and selects an executable plan. It follows the optimization problem formulation (paper, "Optimization Problem Formulation") for the baseline model and labels additional design work **Extension (#1207)**.

The Rust blocks are interface sketches, not compilable code. Paths name today's code; issue numbers name the work that changes it.

## 1. Model and running example

A **workload** contains query expressions, their evaluation schedules, and their accuracy targets. A **memo group** represents one logical result or one intermediate state family over specified input data. A **candidate** is one way to compute that result: one operator over input groups, together with its properties.

Input groups leave producer choices open. Selecting a candidate does not select its entire input plan. Each consumer input chooses a producer separately, and several inputs may choose the same producer.

The expression includes the operator's arguments, including the summary family. In this design, KLL and DDSketch states belong to separate state groups. A final-result group can offer readout candidates that use either family. So the decision tree's first level (Section 2) chooses the expression, family included; a state group never mixes families.

### Running example

Query A requests the p95 latency over one minute with a rank-error target. Query B requests p99 divided by p50 over the same input with a relative-error target. Both run every ten seconds.

| Group | Result | Candidate expressions |
|---|---|---|
| G1 | Raw latency input | `Scan latency[1m]` |
| G2 | KLL state over G1 | `SummaryAgg[KLL](G1)` with `k = 200` or `k = 345` |
| G2′ | DDSketch state over G1 | `SummaryAgg[DDSketch](G1)` with `alpha = 0.01` or `alpha = 0.02` |
| G3 | Query A output | `SummaryEstimate[q=0.95](G2)` |
| G4 | p99 latency | `SummaryEstimate[q=0.99](G2′)` |
| G5 | p50 latency | `SummaryEstimate[q=0.50](G2′)` |
| G6 | Query B output | `Divide(G4, G5)` |

KLL and DDSketch states are separate groups because the family is part of the expression. The metric rule of Section 4 decides which consumer may read which: KLL bounds **rank** error, which does not bound a quantile's value error or a ratio's relative error, so B's operands read the DDSketch state (relative value error), and A reads the KLL state.

G4 and G5 choose a G2′ producer independently. If both choose the same retained physical producer, they reuse one computed state when their schedules and windows are compatible. An unmaterialized candidate may instead execute separately in different contexts; Section 5 explains that distinction.

### Notation used by the baseline model

Symbols are collected here and in the sections that use equations. The prose and code use descriptive names.

| Symbol | Meaning |
|---|---|
| $\mathcal W$, $\mathcal Q$, $H$ | Workload, query set, and common evaluation horizon |
| $\mathcal T_q$, $A_q$ | Query evaluation schedule and accuracy target |
| $G$, $G_q$ | Memo group and query-output group |
| $v$, $u$, $c$, $i$ | Candidate, producer candidate, consumer candidate, and consumer input index |
| $e_v$, $p_v$ | Candidate expression and properties |
| $\boldsymbol\theta_v$, $\tau_v$, $\ell_v$ | Parameter configuration, execution timing, and result lifetime |
| $\mathcal I(c)$, $G_{c,i}$ | Consumer input indices and required group for one input |
| $r_{c,i}$ | Logical and physical requirements of one input |
| $\mathcal C$ | Physical candidate set |
| $\alpha^{\mathrm{log}}_{u,c,i}$, $\alpha^{\mathrm{phys}}_{u,c,i}$ | Fixed logical and physical compatibility indicators |

The candidate and property definitions are:

$$
v=(e_v,p_v), \qquad e_v=\operatorname{op}_v(G_{v,1},\ldots,G_{v,n_v}).
$$

$$
p_v=(p_v^{\mathrm{log}},p_v^{\mathrm{phys}}),\qquad
p_v^{\mathrm{log}}=\boldsymbol\theta_v,\qquad
p_v^{\mathrm{phys}}=(\tau_v,\ell_v).
$$

The formulation's text defines the producer set after the requirement check, and its notation table before it. This document follows the table and constraints (G)–(H), and names the filtered set separately:

$$
P_{c,i}=G_{c,i}\cap\mathcal C,
\qquad
P^{\mathrm{adm}}_{c,i}
=\{u\in P_{c,i}:\alpha^{\mathrm{log}}_{u,c,i}=\alpha^{\mathrm{phys}}_{u,c,i}=1\}.
$$

$$
\mathcal E=\{(u,c,i):c\in\mathcal C,\ i\in\mathcal I(c),\ u\in P_{c,i}\}.
$$

Here, group membership denotes the physical candidates belonging to that group. Property satisfaction means that both checks succeed; it is not a universal numerical ordering.

## 2. Properties and input requirements

A property describes what a candidate delivers. An input requirement describes what a consumer can accept. Workload requirements such as final accuracy and latency apply to the composed plan.

### Property dimensions

This is the single reference table for property dimensions. Stage descriptions below explain when these choices are made without repeating their definitions.

| Dimension | Values and meaning | Code reference or target |
|---|---|---|
| Parameters | A predefined configuration list per family: KLL and KMV `k`; DDSketch `alpha`; HLL `precision`; CMS and CountSketch `width` and `depth`; heap variants also `heap_size`; UnivMon its sizing parameters. The lists are today's sizing over ε = 0.1 down to 0.001; KMV's and UnivMon's distinct-count entries stop at ε = 0.01. A heap variant combines each width × depth with heap sizes from the 1-2-5 series 10, 20, 50, …, 10 000: a top-k consumer's floor is heap_size ≥ k, and the smallest listed heap that meets it is the next step up, at most 2.5× too large. A sketch build's parameters must be in its family's list; exact operators have no sketch parameters. Hydra and the other families Pass 1 does not size have empty `parameter_list`s, so no build of theirs is a valid candidate until their error models exist (#1228). | `SketchParams` in `crates/types/src/ir/schema/sketch.rs`; predefined lists are the #1203 target |
| Window layout — Extension | No layout, or tumbling windows with a positive width. Every grid is anchored at phase 0 (the epoch), so the width alone fixes it. Candidate widths come from the gcd closure of the queries' ranges, steps and phases. Sliding merge/subtraction layouts remain future work. Window count is derived from range and width; retention amount is selected later. | `WindowLayout`; `pass2/memo/window_candidates.rs`; #1160 |
| Timing | `IngestionTime` or `QueryTime` | `ExecutionTiming` |
| Lifetime | `None`: discard after the consuming execution; `OneEvaluation`: retain until consumers within that evaluation finish; `AcrossEvaluations`: retain across evaluations in the specified period. A non-`None` lifetime means materialized. | `Lifetime`; #1200 / #1214 |
| Storage medium — Extension | `Memory` or `Disk`, carried by a materialized lifetime | `StorageMedium`; #1214 |

Error guarantees are derived from parameters and the statistic being read. They include an error metric, a bound, and a failure probability; exact computation introduces no approximation error. The same state may support different readouts, so its guarantee must be interpreted for the relevant statistic.

The extended logical properties add the window layout; the physical properties stay the formulation's:

$$
p_v^{\mathrm{log}}=(\boldsymbol\theta_v,\omega_v),\qquad
p_v^{\mathrm{phys}}=(\tau_v,\ell_v).
$$

### Logical requirements

The proposed `LogicalRequirements` stores a **parameter floor** $\boldsymbol\theta^{\min}$: the weakest parameters whose ε is at most the applicable input error cap, or a direct need such as heap_size ≥ k for a top-k. The floor is any value of the family's parameter type and need not be in the family's list; a listed producer satisfies it when $\boldsymbol\theta_u \succeq \boldsymbol\theta^{\min}$. Section 4 defines the cap and distinguishes validity from pruning. It does not allocate a fixed share of the query's error budget to each input.

For KLL, a larger `k` satisfies a smaller minimum `k`. For DDSketch, a smaller `alpha` can be stronger. Multi-parameter families require the family's comparison rule, including parameters needed for failure guarantees. A single floor is valid only if the accepted configurations form an upward-closed set with a representable least element. Otherwise, compatibility must use an explicit admissible set or predicate; it must not invent a unique minimum.

The required input group's identity already constrains the state family and data/window semantics. An exact producer satisfies an approximation floor only if it is also a valid producer of that group and input type.

### Physical requirements

Physical requirements are derived from the consumer's timing and execution context, rather than stored redundantly. Stage 2 checks timing against lifetime (the table below). Stage 3 checks alignment per context (below), with the schedule of the context the consumer runs in.

| Consumer timing | QT, None | QT, OneEvaluation | QT, AcrossEvaluations | IT, None | IT, AcrossEvaluations |
|---|---|---|---|---|---|
| QueryTime | Allowed | Allowed | Allowed | Rejected | Allowed |
| IngestionTime | Rejected | Rejected | Rejected | Allowed | Allowed |

QT means QueryTime; IT means IngestionTime. Alignment, data coverage, and retention checks still apply to allowed combinations.

**Alignment.** There is no physical schedule property. When a result can be read follows from its lifetime and window layout:
- **None** and **OneEvaluation**: the result is computed inside one evaluation context κ (Section 5) and read there. A OneEvaluation result belongs to one context: queries with equal recurrence and range evaluation are evaluated together, and that evaluation's consumers share the result. When it runs is the context's schedule, and Stage 3 chooses it through $x_{\kappa,v}$, just as for ℓ = None. Sharing across differently scheduled queries through a gcd schedule is not offered: it is rarely beneficial and wastes runs.
- **Windowed AcrossEvaluations**: the input is cut into tumbling windows [k·w, (k+1)·w), with w from the window layout ω and the grid anchored at phase 0 (the epoch). Each window is built when it ends and can be read from then on. A consumer can read a range that ends on a window boundary. With w = 10 s, a `[1m]` query evaluated every 30 s at phase 0 merges the newest 6 windows; one evaluated at phase 15 s cannot, because its range ends inside a window; with that query in the workload, the gcd closure also offers w = 5 s, which both can read.

A grid phase is not needed: a phase is absorbed by the width, because width candidates come from the gcd closure of the queries' ranges, steps and phases. For example, phases 0 and 5 s with w = 10 s give w = 5 s. Alignment alone does not ensure sufficient retained history; that is $h_v$ (Section 5).

An evaluation context κ is described by the workload's own recurrence and range evaluation (Section 5). Let a windowed producer have tumbling width $w$. A consumer running in κ can read the producer when $w$ divides every evaluation time of κ:

| Producer lifetime, or κ's recurrence for a windowed AcrossEvaluations producer | Required alignment |
|---|---|
| None | Always: runs in κ |
| OneEvaluation | The result belongs to κ |
| `FixedIntervalAt { interval, evaluation_phase }`, range step $s$ | $w \mid \gcd(\text{interval}, s, \text{phase})$, the evaluation grid; $s$ counts only for a range evaluation of more than one step |
| `Scheduled(times)` | $w$ divides every time (and $s$) |
| `OneTime { execute_at: Some(t) }` | $w \mid t$ (and $s$) |
| `FixedInterval` without a phase, `OneTime { execute_at: None }`, `EstimatedRate`, `Unknown` | Never: the evaluation times are not known |
| AcrossEvaluations, no window layout | Not offered yet: it needs its own delivery rule |

The evaluation grid is `QueryRecurrence::evaluation_grid_ms(range_evaluation)` in `crates/types/src/workload/mod.rs`, which the workload entries also use. One-off queries are supported: each is a context of its own recurrence, run `invocations` times.

### Invalid combinations

The checked constructor rejects one combination:

| Combination | Reason |
|---|---|
| IngestionTime with OneEvaluation | No query evaluation lifetime at ingestion time |

A tumbling grid's width is the candidate's logical window layout $\omega_v$ and its phase is 0, so no physical field repeats or extends it.

### Decision tree and order of decisions

A memo group's candidates are the leaves of a tree. The first level chooses the expression $e_v$ (operator and family); each later level decides one property dimension. A path from the root to a leaf is one physical candidate.

```mermaid
flowchart TD
    G["Memo group G<br/>(logical result)"] --> R1["e_v: exact operator"]
    G --> R2["e_v: readout of KLL state"]
    G --> R3["e_v: readout of DDSketch state"]
    G --> R4["e_v: other operators<br/>(prune: executor support)"]

    R2 --> T1["θ: k = 200"]
    R2 --> T2["θ: k = 345"]
    R2 --> T3["θ: k = …<br/>(prune: pruning cap)"]

    T2 --> W1["ω: no windows"]
    T2 --> W2["ω: tumbling, width w<br/>from the gcd closure of ranges,<br/>steps and phases (prune: w must<br/>divide the group's query range)"]

    W2 --> P1["τ = QueryTime"]
    W2 --> P2["τ = IngestionTime<br/>(prune: no ingestion pipeline)"]

    P1 --> L1["ℓ = None"]
    P1 --> L2["ℓ = OneEvaluation"]
    P1 --> L3["ℓ = AcrossEvaluations<br/>(prune: deployment keeps<br/>no query-time results)"]
    P2 --> L4["ℓ = None"]
    P2 --> L5["ℓ = AcrossEvaluations"]
    P2 -. invalid .-> L6["ℓ = OneEvaluation"]
```

| Level | Decides | Stage | Values offered | Pruned when |
|---|---|---|---|---|
| 1 | expression $e_v$ | Pass 1 and Pass 2 rules | each way to compute the group | the executor cannot build the family |
| 2 | $\boldsymbol\theta_v$ | Pass 1 | the family's predefined list | the pruning cap (Section 4) |
| 3 | $\omega_v$ — Extension | Pass 2 | tumbling widths from the gcd closure of the queries' ranges, steps and phases | the width does not divide the group's query range |
| 4 | $\tau_v$ | Stage 2 | QueryTime, IngestionTime | the source has no ingestion pipeline |
| 5 | $\ell_v$ | Stage 2 | None, OneEvaluation, AcrossEvaluations | invalid with $\tau_v$; the deployment keeps no query-time results |

The tree does not decide which producer each consumer input reads ($y$), in which context a candidate with ℓ = None or OneEvaluation runs (Section 5), or how much an AcrossEvaluations candidate retains ($h_v$). Those are Stage 3 choices.

**The order does not change the candidate set.** The leaves are the product of the dimensions' values minus invalid combinations and pruned values. Each prune rule reads only the value it removes and values decided above it, so any order that keeps each prune rule below the dimensions it reads yields the same leaves.

**The order changes how many nodes enumeration visits:** $\sum_d \prod_{d' \le d} \lvert \text{values left at } d' \rvert$. Dimensions that prune much go early; dimensions that multiply without pruning go late. The order above follows four rules:

1. **Logical before physical.** Stage 1 hash-conses groups and computes $\alpha^{\log}$ once, before Stage 2 multiplies each logical candidate by its physical alternatives.
2. **Strong prunes early.** Executor support and the pruning cap act before windows and physical properties multiply the tree.
3. **Dependent dimensions adjacent.** $\tau \to \ell$ rejects the invalid combination at the first level that can detect it.
4. **Deployment-wide prunes as early as possible.** Without an ingestion pipeline the IngestionTime subtree is pruned once per candidate; moving $\tau$ above $\omega$ would prune it once per group, with the same leaves.

Alternative orders worth measuring: $\tau$ first, when most sources lack an ingestion pipeline; $\omega$ before $\boldsymbol\theta$, when queries' ranges prune many widths; and requirement-driven (top-down) enumeration, which visits fewer nodes but must take the union of all consumers' requirements so that sharing is not lost.

## 3. Pipeline: interfaces and rules

| Stage | Input | Output | Responsibility |
|---|---|---|---|
| Stage 0 | Query IR | `Memo` (groups without candidates) | Hash-cons logical expressions |
| Stage 1, Pass 1 | Group DAG | `Memo` | Add initial logical candidates and intermediate groups |
| Stage 1, Pass 2 | Memo | `LogicalASAPMemoCandidates` | Add reuse alternatives and compute logical edges |
| Stage 2 | Logical memo candidates | `PhysicalASAPMemoCandidates` | Expand physical alternatives and compute compatibility |
| Stage 3 | Physical memo candidates | `DagOfSelectedCandidates` | Select implementations, dependencies, and executions |
| Realizer/export | Selected DAG | `PhysicalASAPDAG` | Build and export the executable graph |

Enumeration follows the decision tree in Section 2. Accuracy pruning uses only the caps of Section 4.

Rules add groups through `find_or_add_group` and candidates through `add_candidate`. They match group expressions, avoid duplicating an existing expression/property pair, and do not copy consumers to make a new producer visible. After logical generation finishes, edge construction records the possible producer choices.

### Stage 0: groups

`memo_of_queries` in `crates/stage0-memo` builds the groups bottom-up from the queries' nodes: each node's operator, over its children's groups, goes through `Memo::find_or_add_group`, so identical sub-expressions are one group; each query's root is then added with `Memo::add_query`. Stage 0 and Stage 1 share one type, `Memo`: Stage 0 returns it with every group's candidate list empty, and Stage 1 fills the lists.

```rust
pub struct MemoGroupId(pub u32);

pub struct MemoGroup {
    /// What it computes: one operator with its arguments. Each `MemoGroupId`
    /// in it is an input group, never this group's own id.
    pub expression: Operator<MemoGroupId>,
    /// Derived once when the group is created, from its input groups'
    /// `expression` and `group_properties` (optd/TiDB-style); never changes.
    pub group_properties: GroupProperties,
    pub parents: Vec<MemoGroupId>,
    pub queries: Vec<usize>,
    // Empty after Stage 0; populated by Stage 1.
    pub candidates: Vec<CandidateForMemoGroup>,
}

pub struct GroupProperties {
    pub schema: Schema,
    pub result_kind: OperatorResultKind,
    /// What a summary-state group covers; `None` for any other group.
    pub summary_coverage: Option<SummaryCoverage>,
    /// Whether two equal expressions return the same rows for the same
    /// input rows, so they may be one group (not an unordered `LIMIT`).
    pub shareable_when_equal: bool,
}

/// What a summary state computes, with every lifted selection removed.
pub struct SummaryStateDefinition {
    /// The group the operators below read: the first input under the
    /// SummaryAgg that is not Filter / range TimeRange / TimeShift without @
    /// / direct-column Project.
    pub input_group: MemoGroupId,
    /// The operators not lifted into the selection, listed top down from
    /// the SummaryAgg, each with only its residual arguments. `None` marks
    /// an operator's main input (the next unlifted operator, or input_group);
    /// `Some(g)` is another input group it reads, such as the group a scalar
    /// subquery in a Filter reads. Two states may merge only if input_group
    /// and these operators are equal.
    pub unlifted_operators: Vec<Operator<Option<MemoGroupId>>>,
}

pub struct SummaryCoverage {
    pub(crate) definition: SummaryStateDefinition,
    /// Union of boxes over the output rows of the operators' input.
    pub(crate) selection: Vec<SelectionBox>,
}

impl SummaryCoverage {
    pub(crate) fn derive_from_inputs(
        operator: &Operator<MemoGroupAsInput<'_>>,
    ) -> Result<Self, CoverageError>;
    /// A hash of the `SummaryStateDefinition`; equal definitions in one
    /// memo get equal ids.
    pub(crate) fn definition_id(&self) -> String;
}

pub struct Memo {
    // Private: groups are added only through `find_or_add_group`, so the
    // hash-consing index covers every group.
    groups: Vec<MemoGroup>,
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    // Private: the groups `find_or_add_group` may return, by expression hash.
    groups_by_expression_hash: HashMap<u64, Vec<MemoGroupId>>,
}

impl Memo {
    pub fn groups(&self) -> &[MemoGroup];
    /// Adds a query with this root; its groups and those below list it.
    pub fn add_query(&mut self, root: QueryRoot<MemoGroupId>) -> usize;
}
```

How the definition and selection are derived, with examples: `docs/design_docs/proposals/asap-primitive-schema.md` §4.2.2.

The rules read an operator's inputs through one trait, so the same rules derive a front-end node's schema and a group's `GroupProperties`:

```rust
pub(crate) trait OperatorInput: Sized {
    fn schema(&self) -> &Schema;
    fn result_kind(&self) -> OperatorResultKind;
    /// The input's own operator, its children as inputs again.
    fn operator(&self) -> Operator<Self>;
    fn summary_coverage(&self) -> Option<&SummaryCoverage>;
}
// Front-end node trees; summary_coverage is None.
impl OperatorInput for Rc<OperatorNode> { … }
/// A memo group read as an input: its group_properties and expression.
pub(crate) struct MemoGroupAsInput<'m> { memo: &'m Memo, group_id: MemoGroupId }
impl OperatorInput for MemoGroupAsInput<'_> { … }

// The existing rules, generic over the input; existing names kept.
impl<I: OperatorInput> Operator<I> {
    pub(crate) fn output_schema(&self) -> Result<Schema, SchemaDerivationError>;
    pub(crate) fn output_kind(&self) -> OperatorResultKind;
    pub(crate) fn validate_inputs(&self) -> Result<(), SchemaDerivationError>;
    pub(crate) fn is_shareable_when_equal(&self) -> bool;
}
fn derive_group_properties(
    memo: &Memo,
    expression: &Operator<MemoGroupId>,
) -> Result<GroupProperties, SchemaDerivationError>;
```

Summary coverage exists only in the memo: a node tree has none, and `SummaryCoverage` has no type parameter. The executor reads only `definition_id` from the plan.

A group is its expression: one operator, with its arguments, over input groups. Hash-consing compares and hashes the expression directly, so two expressions are one group when their operators and arguments are equal and their input groups are the same. θ is not an operator argument, so builds that differ only in θ share one group. An expression whose copies may return different rows (an unordered `LIMIT`) always gets a new group.

Everything that was computed on operator-node trees is computed on groups. `find_or_add_group` derives `GroupProperties` once, when the group is created, with `derive_group_properties`: the rules that check operator nodes (schema derivation, input validation, merge check), applied to the expression over `MemoGroupAsInput`s, which read only the input groups' `expression` and `group_properties`, never a node tree:

- `schema` and `result_kind` from the operator and its input groups' schemas and result kinds; a scalar subquery reads the referenced group's schema.
- `summary_coverage`, for a summary state: walking down from the SummaryAgg through the input groups' expressions while each is a Filter, a range TimeRange, a TimeShift without `@` or a direct-column Project, lifting their predicates into the selection. The group below them is the definition's `input_group`; a Scan is never lifted through, so its predicates stay in that group. The operators not lifted, each with its residual arguments, are `unlifted_operators`. A tumbling-window chain covers what one build over its whole window does. A merge's coverage comes from its inputs' stored coverage; a merge is valid only over inputs with equal definitions and disjoint selections.
- The checks that read an input's operator (a merge of the newest windows reads a window chain; a selected build) read the input group's `expression`.
- `shareable_when_equal`: a `LIMIT` is shareable over a `Sort` with the same partitions (the input group's `expression`); a `Dedup` on every column (the input group's schema).

`add_candidate` derives a candidate's properties with the same `derive_group_properties` and requires its schema and result kind to equal the group's. `OperatorNode` recursively contains chosen child nodes and therefore describes a fixed plan fragment; `Operator<MemoGroupId>` contains input group IDs and leaves their implementations open. A candidate needs its own expression because a group may offer several ways to compute the same result.

Removing the one-field `CandidateExpression` wrapper is possible but not adopted.

#### Column lineage

Every output column of a group has a **lineage**: the source column its values are copied from, or none when the column is computed. It is a group property, derived once from the input groups' lineage (as optd's column-reference property is), and it replaces the table qualifier that schema fields carried. A schema field keeps only its name, type and nullability; a front end resolves a qualified name such as `f.src` with its own binding scope while it lowers the query, and the IR never needs the qualifier again.

```rust
pub struct GroupProperties {
    pub schema: Schema,                          // fields: name, type, nullability; no table qualifier
    pub result_kind: OperatorResultKind,
    /// One entry per output column, in schema order.
    pub column_lineage: Vec<ColumnLineage>,
    pub summary_coverage: Option<SummaryCoverage>,
    pub shareable_when_equal: bool,
}

/// Where an output column's values come from.
pub enum ColumnLineage {
    /// Copied unchanged from a column of a source (a Scan's table or metric).
    SourceColumn { source: String, column: String },
    /// Computed by an operator: a measure, an expression, a summary state or readout.
    Computed,
}
```

**Derivation, per operator** (from the input groups' `column_lineage`):

| Operator | Output column lineage |
|---|---|
| `Scan` | each column: `SourceColumn { source, column }` |
| `Filter`, `TimeRange`, `TimeShift`, `Sort`, `Limit`, `Dedup` | the input's, unchanged |
| `Project` | a column item, renamed or not: that input column's lineage; an expression item: `Computed` |
| `Aggregate`, `SummaryAgg`, readouts | group-by keys: the input's lineage of that key; measures, states and values: `Computed` |
| `Join` | the left input's columns, then the right input's |
| `SetOp`, `Concat` | per column: the inputs' lineage if they all agree, else `Computed` |
| `BinaryOp` (vector match) | label columns: the matched side's lineage; the value: `Computed` |

**What reads it:**
- **Summary coverage.** A selection constrains columns identified by lineage (`ColumnIdentity` is a `SourceColumn`), not by (qualifier, name). A predicate on a `Computed` column is not lifted; it stays in `unlifted_operators`. A merge's group keys must have equal lineage.
- **Candidate checks.** `add_candidate` compares a candidate's schema with its group's exactly: names, types, nullability, order. There is no qualifier to compare, so a readout renamed to the group's column name matches.

**Example 1: a SQL aggregate and its summary candidate.**

```sql
SELECT f.src, approx_distinct(f.dst) FROM flows f WHERE f.proto = 'tcp' GROUP BY f.src
```

| Group | Expression | Output columns | `column_lineage` |
|---|---|---|---|
| G1 | `Scan flows` | `src, dst, proto, ts` | `flows.src, flows.dst, flows.proto, flows.ts` |
| G2 | `Filter(proto = 'tcp')(G1)` | `src, dst, proto, ts` | same as G1 |
| G3 | `Aggregate(by src; approx_distinct(dst))(G2)` | `src, approx_distinct(dst)` | `flows.src, Computed` |
| G4 | `SummaryAgg[HLL](by src; dst)(G2)` | `src, state` | `flows.src, Computed` |
| G5 | `SummaryEstimate[cardinality](G4)` | `src, cardinality` | `flows.src, Computed` |
| — | candidate of G3: `Project(src, cardinality AS approx_distinct(dst))(G5)` | `src, approx_distinct(dst)` | `flows.src, Computed` |

The candidate's schema equals G3's, so `add_candidate` accepts it. Before lineage, G3's key carried the qualifier `flows` and the renaming `Project` could not keep it on one column while renaming another, so this candidate was rejected.

**Example 2: renames keep lineage.**

```sql
SELECT source, count(*) FROM (SELECT f.src AS source FROM flows f) GROUP BY source
```

The inner `Project` renames `src` to `source`; its lineage stays `flows.src`. Two summary states, one grouped by `source` here and one grouped by `src` in another query, have keys of equal lineage, so a merge of the two may pass the group-key check. Compared by (qualifier, name), `source` and `flows.src` looked like different columns.

**Example 3: the same name from two sources.**

```sql
SELECT s.region, count(*) FROM sales s JOIN users u ON s.user = u.id GROUP BY s.region
```

After the join, `region` may exist on both sides. The front end resolves `s.region` to the left input's column while lowering; in the IR the two columns have lineage `sales.region` and `users.region`, so a selection on `sales.region` is never confused with one on `users.region`.

**Example 4: PromQL label matchers.** `quantile_over_time(0.9, latency{job="api"}[1m])` and the same with `job="web"`. Each Scan's `job` column has lineage `latency.job`. The lifted matchers become selections `latency.job ∈ {api}` and `latency.job ∈ {web}`: disjoint selections on the same column, so the two KLL states may merge.


### Stage 1, Pass 1: initial candidates

Implementation locations: `crates/logical-asap-optimization/src/pass1/per_computation_candidates.rs` and `memo_population.rs`. The #1203 target gives every intermediate result its own group; candidates contain one operator rather than private operator chains.

| Alternative | Added representation |
|---|---|
| Exact | Exact candidate in the result group |
| Exact accumulator | Accumulator-state group plus a finalize candidate in the result group |
| Summary | One state group per supported family; one build candidate per listed parameter configuration; separate readout candidates |
| Fused build (#1203 2c) | Candidate with `fused_groups` identifying additional results; selection must enforce their availability consistently |

```rust
pub struct CandidateForMemoGroupId {
    pub group: MemoGroupId,
    /// Position in the group's `candidates` list.
    pub index_in_group: usize,
}

impl Memo {
    /// The group of `expression` (operator, arguments and input groups),
    /// added if new. Fails when the expression breaks the node rules
    /// (schema derivation, input validation, merge check).
    pub fn find_or_add_group(
        &mut self,
        expression: Operator<MemoGroupId>,
    ) -> Result<MemoGroupId, SchemaDerivationError>;

    /// Adds `candidate` to `group`, or returns the id of the candidate there
    /// with the same expression and logical properties. The candidate's
    /// expression is checked by the same node rules as a group's, and its
    /// output schema and kind must equal the group's; a rejected candidate
    /// leaves the memo unchanged.
    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> Result<CandidateForMemoGroupId, CandidateError>;

    pub fn candidate(&self, id: CandidateForMemoGroupId) -> &CandidateForMemoGroup;
}

pub enum CandidateError {
    /// The candidate's expression fails the node rules (schema derivation,
    /// input validation, merge check).
    Schema(SchemaDerivationError),
    /// Its output schema or output kind differs from the group's: it does
    /// not compute the group's result.
    OutputDiffersFromGroup { group: MemoGroupId },
}

pub struct CandidateExpression {
    pub operator: Operator<MemoGroupId>,
}

pub struct CandidateForMemoGroup {
    pub expression: CandidateExpression,
    pub logical_properties: LogicalProperties,
    pub input_requirements: Vec<LogicalRequirements>,
    pub fused_groups: Vec<MemoGroupId>,
    // Diagnostic provenance; not a planning decision.
    pub origin: OriginOfCandidateForMemoGroup,
}

pub struct LogicalProperties {
    parameters: Option<SketchParams>,
    window_layout: Option<WindowLayout>,
}

pub enum WindowLayout {
    Tumbling { width_ms: u64 },
}

impl LogicalProperties {
    /// Rejects parameters missing on a sketch build, present on an exact
    /// operator, of another family or not in the family's list, and a window
    /// of width 0 or on an operator that cannot be windowed.
    pub fn new(
        expression: &CandidateExpression,
        parameters: Option<SketchParams>,
        window_layout: Option<WindowLayout>,
    ) -> Result<Self, LogicalPropertiesError>;

    pub fn parameters(&self) -> Option<&SketchParams>;
    pub fn window_layout(&self) -> Option<&WindowLayout>;
}

pub fn parameter_list(family: SketchAlgorithm) -> &'static [SketchParams];

pub struct LogicalRequirements {
    parameter_floor_for_required_accuracy: Option<SketchParams>,
}

impl LogicalRequirements {
    /// Any floor of the family's parameter type, listed or not.
    pub fn new(parameter_floor_for_required_accuracy: Option<SketchParams>) -> Self;

    pub fn parameter_floor_for_required_accuracy(&self) -> Option<&SketchParams>;
}

pub fn producer_satisfies_logical_requirements(
    producer: &LogicalProperties,
    requirement: &LogicalRequirements,
) -> bool;
```

The floor representation has the applicability condition stated in Section 2. Creating requirements also needs the input family's configuration domain; a constructor without that context can validate only the information encoded in its argument. Error-model interfaces are grouped with accuracy in Section 4.

Migration targets: remove private `root_node` chains, duplicate `input_groups`, and `accuracy_budget_parts`. The proposed predefined parameter lists replace sizing one candidate per budget partition.

### Stage 1, Pass 2: reuse rules and logical edges

Paths are relative to `crates/logical-asap-optimization/src/pass2/memo/`.

| Rule / function | File | Result or migration target |
|---|---|---|
| Local windows: `add_local_window_composition` | `window_candidates.rs` | Add layouts to the state group; #1160 removes copied window consumers |
| Scan widening: `add_scan_label_candidates` | `rules/scan_labels.rs` | Add a widened scan group and projections in the original groups; #1222 |
| Summary inputs: `factor_summary_inputs` | `rules/summary_inputs.rs` | Expose projections/filters as input groups; #1218 moves this into initial population |
| Top-k reuse: `add_topk_candidates` | `rules/topk.rs` | Read a compatible larger top-k result with an exact-input requirement; #1220 |
| Finer aggregate: `add_finer_aggregate_candidates` | `rules/finer_aggregate.rs` | Filter and roll up finer groups, using separate intermediate groups; #1220 |
| Rollup: `add_rollup_candidates` | `rules/rollup.rs` | Add compatible finer-state merges; #1221 |
| Window rollup: `add_window_rollup_candidates` | `rules/rollup.rs` | Merge shorter windows into longer-range results; #1221 |
| Summary results: `factor_summary_results` | `rules/summary_results.rs` | Removed once Pass 1 exposes readouts; #1218 |
| Shared configurations: `add_state_configuration_candidates` | `rules/shared_configuration_readers.rs` | Remove copied consumers once producer variants are selectable; #1203 2b |
| Shared state groups: `add_shared_summary_state_groups` | `state_groups.rs` | Remove per-size groups; #1203 2b / #1160 |

After the rules, Pass 2 builds the logical edges and applies the pruning caps of Section 4. Edges name the consumer input `input_index_of_consumer`.

```rust
fn apply(memo: &mut Memo) -> Result<(), Pass2RuleError>;

pub struct LogicalEdge {
    pub producer_candidate_id: CandidateForMemoGroupId,
    pub consumer_candidate_id: CandidateForMemoGroupId,
    pub input_index_of_consumer: usize,
    pub satisfies_logical_requirement: bool,
}

pub struct LogicalASAPMemoCandidates {
    pub memo: Memo,
    pub edges: Vec<LogicalEdge>,
}
```

### Stage 2: physical alternatives

`enumerate_candidate_configurations` in `crates/physical-asap-optimization/src/catalog/configurations.rs` expands each logical candidate using the dimensions and checks in Section 2. It creates no new logical candidates. Each logical edge is expanded between the relevant physical alternatives.

```rust
// crates/types/src/physical/properties.rs
pub enum ExecutionTiming {
    IngestionTime,
    QueryTime,
}

pub enum StorageMedium {
    Memory,
    Disk,
}

pub enum Lifetime {
    None,
    OneEvaluation(StorageMedium),
    AcrossEvaluations(StorageMedium),
}
pub struct PhysicalProperties {
    timing: ExecutionTiming,
    lifetime: Lifetime,
}
impl PhysicalProperties {
    /// Rejects only (IngestionTime, OneEvaluation).
    pub fn new(
        timing: ExecutionTiming,
        lifetime: Lifetime,
    ) -> Result<Self, PhysicalPropertiesError>;
    pub fn timing(&self) -> ExecutionTiming;
    pub fn lifetime(&self) -> Lifetime;
    pub fn is_materialized(&self) -> bool;
}

impl PhysicalCandidate {
    /// Whether a consumer running in `context` can read this producer
    /// (Section 2's alignment table): for a windowed AcrossEvaluations
    /// producer, w (read from its window layout) divides every evaluation
    /// time of the context; true for ℓ = None, and for OneEvaluation in the
    /// same context.
    pub fn delivers_on(&self, memo: &Memo, context: &ExecutionContext) -> bool;
}
/// α^phys: timing and lifetime only (Section 2's table). Alignment is
/// checked per context in Stage 3 with `delivers_on`.
pub fn producer_satisfies_physical_requirements(
    producer: &PhysicalCandidate,
    consumer: &PhysicalCandidate,
) -> bool;
pub struct PhysicalCandidateId(pub usize);
pub struct PhysicalEdgeId(pub usize);

pub struct PhysicalCandidate {
    pub logical_candidate_id: CandidateForMemoGroupId,
    pub physical_properties: PhysicalProperties,
}
pub struct PhysicalEdge {
    pub producer_candidate_id: PhysicalCandidateId,
    pub consumer_candidate_id: PhysicalCandidateId,
    pub input_index_of_consumer: usize,
    pub satisfies_logical_requirement: bool,
    pub satisfies_physical_requirement: bool,
}
pub struct PhysicalASAPMemoCandidates {
    pub logical_candidates: LogicalASAPMemoCandidates,
    pub physical_candidates: Vec<PhysicalCandidate>,
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidateKey>>,
}
impl PhysicalASAPMemoCandidates {
    pub fn edges(&self) -> impl Iterator<Item = (PhysicalEdgeId, PhysicalEdge)>;
    pub fn admissible_edges_into(
        &self,
        consumer: PhysicalCandidateId,
        input_index_of_consumer: usize,
    ) -> &[PhysicalEdgeId];
    pub fn admissible_edges_out_of(
        &self,
        producer: PhysicalCandidateId,
    ) -> &[PhysicalEdgeId];
}

// Identifies a rejected alternative by content: it has no allocated candidate ID.
pub struct PhysicalCandidateKey {
    pub logical_candidate_id: CandidateForMemoGroupId,
    pub physical_properties: PhysicalProperties,
}
```

`PhysicalCandidateId` indexes an emitted candidate. `PhysicalCandidateKey` describes an alternative rejected before it received an ID, allowing diagnostics to explain pruning. The admissible-edge accessor returns edge IDs, not the unfiltered producer set from Section 1. `delivers_on` takes a candidate rather than bare properties because a tumbling grid's width lives in the logical window layout.

Migration targets include replacing `Materialization` and duplicated physical fields with checked properties, and replacing `ReaderConstraint::{IncompatibleOutputs, AllowedOutputs}` with compatibility and alignment checks. `ReaderConstraint::{MinimumQueryReaders, HighestSharedQueryMaterialization}` are deleted too; see Section 5.

### Stage 3: baseline selection and realization

The baseline uses the formulation's variables and constraints. Stage 3 adds no candidates or potential dependencies.

| Variable | Meaning |
|---|---|
| $x_v$ | Candidate selected |
| $y_{u,c,i}$ | Producer supplies the specified consumer input |
| $z_{q,v}$ | Candidate supplies the query output |
| $t_v$ | Topological rank |

| Constraint | Meaning | Encoding |
|---|---|---|
| (A) | One producer per selected consumer input | $\sum_{u\in P_{c,i}}y_{u,c,i}=x_c$ |
| (B) | Chosen producers are selected | $y_{u,c,i}\le x_u$ |
| (C) | One output per query | $\sum_{v\in G_q}z_{q,v}=1$ |
| (D) | Output candidates are selected | $z_{q,v}\le x_v$ |
| (E) | Acyclic dependencies | $t_u+1\le t_c+\lvert\mathcal C\rvert(1-y_{u,c,i})$ |
| (F) | Selected candidates are used | $x_v\le\sum_{(v,c,i)\in\mathcal E}y_{v,c,i}+\sum_{q:v\in G_q}z_{q,v}$ |
| (G) | Logical compatibility | $y_{u,c,i}\le\alpha^{\mathrm{log}}_{u,c,i}$ |
| (H) | Physical compatibility | $y_{u,c,i}\le\alpha^{\mathrm{phys}}_{u,c,i}$ |
| (I) | Domains | $x_v,y_{u,c,i},z_{q,v}\in\{0,1\}$; $0\le t_v\le\lvert\mathcal C\rvert-1$ |

Constraints are applied for every relevant candidate, input, query, or potential edge. An implementation may allocate edge variables only on admissible edges, making (G) and (H) implicit without changing the baseline feasible set.

The baseline objective charges each selected candidate a fixed cost over the common horizon:

$$
\min\sum_{v\in\mathcal C}w_vx_v.
$$

The cost includes input reading, the candidate's own computation, and lifetime-dependent retention. It excludes producers' computation, which is charged at those producers. This objective assumes execution frequencies and retention costs are fixed before optimization and independent of the selected consumers. When that assumption does not hold, use the execution extension in Section 5; do not silently reuse the same cost coefficient.

```rust
struct SelectionVariables {
    x: Vec<Variable>,
    y: HashMap<PhysicalEdgeId, Variable>,
    z: HashMap<(QueryId, PhysicalCandidateId), Variable>,
    t: Vec<Variable>,
}

// Schematic constraint-building entry points; signatures omitted.
// input_satisfaction_constraints: (A)
// producer_selection_constraints: (B)
// query_coverage_constraints: (C), (D)
// acyclicity_constraints: (E)
// utilization_constraints: (F)
// Compatibility is enforced when allocating y on admissible edges.

pub struct SelectedEdge {
    pub producer: SelectedNodeId,
    pub consumer: SelectedNodeId,
    pub input_index_of_consumer: usize,
}

pub struct DagOfSelectedCandidates {
    pub candidate_of_node: Vec<PhysicalCandidateId>,
    pub edges: Vec<SelectedEdge>,
    pub node_of_output_reference: Vec<SelectedNodeId>,
}
```

The realizer uses the selected dependencies to build the executable graph. Export carries each node's timing and lifetime. Under the context extension, multiple selected execution nodes may refer to the same physical candidate; realization must preserve their contexts rather than merge them solely by candidate ID.

## 4. Accuracy and safe pruning — Extension (#1207)

The formulation checks logical requirements per edge. This extension also checks composed accuracy at each query root. This section gives the error model and its MILP encoding; error is not a property dimension.

### Local propagation

For a consumer, an error function bounds output error from input errors:

$$
\varepsilon_c\le g_c(\varepsilon_{c,1},\ldots,\varepsilon_{c,n}).
$$

Use **beta** for local input factors and **lambda** only for factors composed to a query root:

$$
g_c(\varepsilon_{c,1},\ldots,\varepsilon_{c,n})
\le\sum_i\beta_{c,i}\varepsilon_{c,i}.
$$

| Operation and metric | Applicable bound or condition |
|---|---|
| Exact pass-through | Preserves the input guarantee only when it preserves the metric's semantics |
| Sum/difference, absolute error | Sum of the operands' absolute error bounds |
| Sum over a known number of values | Sum of their bounds; a common per-value bound can be multiplied by that count |
| Ratio, relative error | Requires nonzero true denominator and denominator relative error below one; bound given below |

A filter does not automatically preserve every accuracy guarantee. Group counts may need metadata beyond the expression syntax. Each factor must follow from the operator's error contract.

**Metric rule.** Every local guarantee states its metric: absolute (in value units, or a multiple of a stream norm), relative, or rank. A factor $\beta_{c,i}$ is defined for one input metric and one output metric. An input whose metric the consumer's error contract cannot convert gives no edge ($\alpha^{\log} = 0$); the conversion is never assumed. For example, a relative-error ratio cannot read a KLL quantile (rank error) unless a declared distribution assumption supplies the conversion.

For a ratio, let the operand relative-error bounds be $\varepsilon_X$ and $\varepsilon_Y$. If the numerator's relative error is defined and the denominator is nonzero, then:

$$
\varepsilon_{X/Y}\le
\frac{\varepsilon_X+\varepsilon_Y}{1-\varepsilon_Y}.
$$

Choosing a fixed validity cap $b<1$ for the denominator gives a linear upper bound within that region:

$$
\varepsilon_Y\le b
\quad\Longrightarrow\quad
\varepsilon_{X/Y}\le\frac{1}{1-b}(\varepsilon_X+\varepsilon_Y).
$$

Both local factors are then `1 / (1 - b)`. This is a sufficient bound, not an exact equality or a claim that all feasible nonlinear plans remain represented.

Interface (approved 2026-11-05):

```rust
pub struct ErrorPropagation {
    /// β_{c,i} per input, for that input's metric.
    pub factor_per_input: Vec<f64>,
    /// b_{c,i} per input: the cap under which the linear bound holds; None
    /// when the bound holds everywhere (e.g. a ratio caps only its denominator).
    pub validity_cap_per_input: Vec<Option<f64>>,
}

impl LogicalRequirements {
    pub fn error_propagation(
        consumer: &Operator<MemoGroupId>,
    ) -> ErrorPropagation;
}
```

`factor_per_input` holds the local β factors and `validity_cap_per_input` the per-input caps, aligned by input index. An operator without an error contract is reported as unsupported; no factor is guessed.

### Query composition and root checks

An **error source** is one local approximation event with a stated metric and guarantee. Let $s$ index such sources in a selected, acyclic plan. Let $\varepsilon_s$ and $\delta_s$ be its bound and failure probability. Define $\lambda_{q,s}$ as the sum, over paths from that source to the query output, of products of local beta factors. Define $a_{q,s}$ as a binary indicator that query q's selected dependency graph uses that source.

The intended root conditions are:

$$
\sum_s\lambda_{q,s}\varepsilon_s a_{q,s}\le\varepsilon_q,
\qquad
\sum_s\delta_s a_{q,s}\le\delta_q.
$$

Here, $\varepsilon_q$ and $\delta_q$ are the query target's error and failure limits. Repeated paths can amplify error and therefore contribute to lambda. A shared failure event is counted once in the union bound; distinct readout events need distinct source identities unless a joint guarantee is available.

**MILP encoding.** Path factors $\lambda_{q,s}$ depend on which edges are selected, so the MILP does not use them directly. It carries an error bound per selected candidate instead, which needs no path enumeration and is the same for every query reading that candidate:

$$
E_c \ge \varepsilon^{\mathrm{loc}}_c + \sum_{i} \beta_{c,i} F_{c,i},
\qquad
F_{c,i} \ge E_u - M_{c,i}\,(1 - y_{u,c,i}) \quad \forall (u,c,i)\in\mathcal E,
$$

$$
D_c \ge \delta^{\mathrm{loc}}_c + \sum_{i} H_{c,i},
\qquad
H_{c,i} \ge D_u - (1 - y_{u,c,i}) \quad \forall (u,c,i)\in\mathcal E,
$$

$$
E_v \le \varepsilon_q + M_q\,(1 - z_{q,v}),
\qquad
D_v \le \delta_q + (1 - z_{q,v})
\qquad \forall q,\ v\in G_q. \tag{K}
$$

- $E_c, F_{c,i}, D_c, H_{c,i} \ge 0$ are continuous. $\varepsilon^{\mathrm{loc}}_c$ and $\delta^{\mathrm{loc}}_c$ are the candidate's own guarantee (zero for an exact operator); $\beta_{c,i}$ and the metric rule come from `error_propagation`.
- $M_{c,i}$ is any upper bound on $E_u$ over the producers of input $i$ (their largest feasible bound); $M_q$ likewise for the root. Exactly one $y_{u,c,i} = 1$ per selected input (constraint (A)), so $F_{c,i}$ equals the chosen producer's error bound.
- A producer reached by two paths contributes on both, which is the worst-case sum. The failure probabilities add by a union bound; a failure event reached twice is counted twice, which is conservative. This affects only the error bounds, not cost: the objective counts each selected candidate once through $x_v$. Being conservative can only make the MILP pass over a cheaper plan that would in fact meet the target; it never accepts one that does not. Counting a shared failure event once needs failure-event identities, left for later.
- Under the context extension (Section 5), the variables are indexed by context, $E_{\kappa,c}$, and the edges by $y_{\kappa,u,c,i}$.

$\lambda_{q,c,i}$ is still used, but only for the pruning caps below, where a fixed lower bound suffices.

### Validity caps and pruning caps

A **validity cap** ensures that a chosen local bound applies. A **pruning cap** removes an alternative only when its nonnegative contribution alone exceeds every query target it could serve. Neither reserves an error-budget share for one input.

For an input whose error can be expressed in the same metric, let $Q_{c,i}$ be the queries that could use it. Let $\lambda_{q,c,i}$ denote that input's query-level factor, and let $b_{c,i}$ denote its validity cap, or infinity when no cap is required. With fixed positive factors, a safe cap is:

$$
\operatorname{cap}_{c,i}
=\min\left(b_{c,i},\ \max_{q\in Q_{c,i}}
\frac{\varepsilon_q}{\lambda_{q,c,i}}\right).
$$

A zero factor gives no pruning restriction for that query. If a factor depends on the eventual plan, use a proven lower bound over all feasible downstream alternatives for this pruning test, or defer pruning. Using an arbitrary path's larger factor could discard a feasible plan. The maximum over queries protects alternatives that can serve at least one query.

Compute the parameter floor from this cap only under the ordering condition in Section 2. Validity information is available when the consumer's error model is established; query-sensitive pruning follows logical generation and reachability analysis. Root checks still couple all contributing errors and determine feasibility.

## 5. Execution contexts and other selection extensions — #1207

These extensions go beyond constraints (A)–(I) and the formulation's fixed-cost assumption. They are separated here so that the baseline reads on its own.

### Execution identity

An AcrossEvaluations candidate has its own computation: each window is built when it ends. A candidate with ℓ = None runs inside an execution that needs it. For example, a finalize candidate used by queries A (every 10 s) and C (every 15 s) may execute in both evaluation contexts even though Stage 2 enumerates it only once. A OneEvaluation candidate also runs inside one evaluation context κ, and that evaluation's consumers share the result.

A **context** identifies a class of executions with a common owner and schedule across the accounting horizon. It is not necessarily one timestamp. An evaluation context reuses the workload's own description of when queries run, their `QueryRecurrence` and `RangeEvaluation`: queries with equal ones are evaluated together and share its OneEvaluation results. Queries A and B of the running example, both every 10 s at phase 0, form one context; a one-off query forms the context of its `OneTime` recurrence. `QueryRecurrence` holds an `f64` (`EstimatedRate`) and cannot be hashed, so contexts are deduplicated by equality and `ExecutionContextId` indexes the list of distinct contexts.

```rust
pub struct ExecutionContextId(pub usize);

pub enum ExecutionContext {
    Evaluation {
        recurrence: QueryRecurrence,
        range_evaluation: Option<RangeEvaluation>,
    },
    IngestionPipeline(RefreshWindowId),
    Materialized(PhysicalCandidateId),
}
```

A chain of ℓ = None and OneEvaluation candidates inherits its consuming context. Encountering an AcrossEvaluations producer switches to that producer's own computation context when traversing its inputs. The context identifies where the operation runs; `delivers_on` with the context decides whether a windowed producer can supply it.

| Extended variable | Meaning |
|---|---|
| $x_v^{\mathrm{mat}}$ | AcrossEvaluations candidate selected |
| $x_{\kappa,v}$ | Candidate with ℓ = None or OneEvaluation executes in context kappa |
| $y_{\kappa,u,c,i}$ | Input edge selected for a consumer execution in context kappa |
| $h_v$ | Selected retention amount for an AcrossEvaluations producer |

The unindexed edge variable belongs to the baseline only. In the extension, every edge is indexed by its consumer's context, including the dedicated computation context of an AcrossEvaluations consumer. This avoids silently switching between two meanings of `y`.

Activation, input satisfaction, utilization, output selection, and acyclicity must be defined for these execution instances. One binary per context/candidate assumes at most one reusable execution of that candidate within that context. If two uses require independent computations, the context or execution identity must distinguish them.

### Cost and retention

To avoid confusing per-execution costs with the baseline's horizon-wide cost, use separate coefficient names:

| Symbol | Definition |
|---|---|
| $W_v^{\mathrm{mat}}$ | Fixed computation and access cost over the horizon for an AcrossEvaluations candidate, one build per window, excluding variable retention |
| $c_{\kappa,v}$ | Cost of one execution in the specified context of a candidate with ℓ = None or OneEvaluation, including a OneEvaluation result's storage within that evaluation |
| $\rho_\kappa$ | Fixed number or expected number of context executions over the common horizon, from the existing evaluation counting in `crates/cost/src/execution_demand.rs` (a one-off query's is its `invocations`) |
| $h_v$ | Retention amount in a declared unit, such as retained windows |
| $\pi_v$ | Horizon-wide retention cost per unit of retained amount |
| $\operatorname{need}_{\kappa,v,c,i}$ | Retention amount producer v needs to serve this consumer input in this context: the time horizon the queries served by that context read, e.g. a 5m range over 10 s tumbling windows needs 30 windows. Query latency is ignored for now (decided 2026-11-06) |

A proposed linear objective is:

$$
\min\left(
\sum_{v\text{ across evaluations}}W_v^{\mathrm{mat}}x_v^{\mathrm{mat}}
+\sum_{\kappa,\ v\text{ None or OneEvaluation}}\rho_\kappa c_{\kappa,v}x_{\kappa,v}
+\sum_{v\text{ across evaluations}}\pi_v h_v
\right).
$$

A selected input edge imposes the corresponding retention lower bound:

$$
h_v\ge\operatorname{need}_{\kappa,v,c,i}\,y_{\kappa,v,c,i}.
$$

The need is fixed per edge from the time horizon of the queries it serves, so constraint (R) is linear. It ignores query latency: a result is assumed read as soon as its evaluation starts, so a slow consumer that reads later does not extend the horizon. Modeling that needs the latency design, which is out of scope (Section 5). The retention unit (windows, or seconds of data) and an upper bound on $h_v$ are fixed per candidate when Stage 2 builds it.

### No consumer-count policies

Today's `ReaderConstraint` (`crates/types/src/stages/physical/graph.rs`) is deleted with all four variants (decided 2026-11-06). `IncompatibleOutputs` and `AllowedOutputs` are replaced by the alignment check of Section 2. `MinimumQueryReaders` (keep a candidate only if enough queries reach it) and `HighestSharedQueryMaterialization` (materialize where sharing stops) are left to the cost model: materializing for one query, or below the point where sharing stops, costs more and is not chosen. There is no constraint (J).

### Out of this design

Latency bounds (L) and resource budgets need an execution and critical-path model, and accounting units, that this document does not define. They are left to a later design; no interface for them is part of this one.

## Appendix A. Enumerating DAGs for debugging

This is an inspection tool for small memos, not a planning stage. The MILP selects a plan without listing all alternatives.

A logical DAG has one selected root candidate, one compatible producer per input of every reached candidate, no unused nodes, and no cycles. Producer choices are per consumer input, not per group. Two consumers can choose different candidates of the same group; choosing the same candidate shares one logical node.

Rust-style pseudocode follows. Helpers such as `cartesian_product` are schematic; `reaches` follows the stored consumer-to-producer dependency direction and is reflexive. This direction makes the cycle test precise.

```rust
/// One logical DAG: its nodes and, for each node's inputs, the chosen producer.
#[derive(Clone)]
struct LogicalDag {
    root: CandidateForMemoGroupId,
    /// For each node c in the DAG, one producer per input i, in input order.
    producers: BTreeMap<CandidateForMemoGroupId, Vec<CandidateForMemoGroupId>>,
}

/// Every logical DAG rooted at `root_group`, each exactly once.
fn logical_dags(memo: &Memo, root_group: MemoGroupId) -> Vec<LogicalDag> {
    let mut all = Vec::new();
    for root in candidates_of(memo, root_group) {
        let dag = LogicalDag { root, producers: BTreeMap::new() };
        expand(memo, dag, VecDeque::from([root]), &mut all);
    }
    all
}

/// Choose producers for the inputs of every node in `pending`, in FIFO order.
/// Each DAG is reached by exactly one sequence of choices, so none repeats.
fn expand(
    memo: &Memo,
    dag: LogicalDag,
    mut pending: VecDeque<CandidateForMemoGroupId>,
    all: &mut Vec<LogicalDag>,
) {
    let Some(consumer) = pending.pop_front() else {
        all.push(dag); // Every reached input has a producer.
        return;
    };
    let candidate = memo.candidate(consumer);
    let inputs: Vec<MemoGroupId> = candidate.expression.operator.children().copied().collect();
    // Every way to choose one producer per input of `consumer`: the product over i
    // of the admissible producers in G_{c,i}.
    for choice in cartesian_product(inputs.iter().enumerate().map(|(i, &group)| {
        candidates_of(memo, group)
            .filter(|&u| {
                producer_satisfies_logical_requirements(
                    &memo.candidate(u).logical_properties,
                    &candidate.input_requirements[i],
                )
            })
            .collect::<Vec<_>>()
    })) {
        // In the stored consumer-to-producer direction, this would close a cycle.
        if choice.iter().any(|&u| reaches(&dag, u, consumer)) {
            continue;
        }
        let mut next = dag.clone();
        next.producers.insert(consumer, choice.clone());
        let mut next_pending = pending.clone();
        for &u in &choice {
            // A producer already in the DAG is shared, not expanded again.
            if !next.producers.contains_key(&u) && !next_pending.contains(&u) && u != next.root {
                next_pending.push_back(u);
            }
        }
        expand(memo, next, next_pending, all);
    }
}

fn candidates_of(
    memo: &Memo,
    group: MemoGroupId,
) -> impl Iterator<Item = CandidateForMemoGroupId> + '_ {
    (0..memo.groups()[group.0 as usize].candidates.len())
        .map(move |index_in_group| CandidateForMemoGroupId { group, index_in_group })
}

/// Whether `from` reaches `to` through the producers chosen so far
/// (true when `from == to`, so a candidate never reads itself).
fn reaches(dag: &LogicalDag, from: CandidateForMemoGroupId, to: CandidateForMemoGroupId) -> bool;

/// Print one DAG as an indented tree; a node read a second time is printed as a
/// reference to its first occurrence, so sharing is visible.
fn print(memo: &Memo, dag: &LogicalDag) {
    fn node(
        memo: &Memo,
        dag: &LogicalDag,
        c: CandidateForMemoGroupId,
        depth: usize,
        seen: &mut HashSet<CandidateForMemoGroupId>,
    ) {
        let candidate = memo.candidate(c);
        if !seen.insert(c) {
            println!("{}↑ G{}[{}] (shared)", "  ".repeat(depth), c.group.0, c.index_in_group);
            return;
        }
        println!("{}G{}[{}] {} {:?}", "  ".repeat(depth), c.group.0, c.index_in_group,
                 candidate.expression.operator.name(), candidate.logical_properties);
        for &u in dag.producers.get(&c).into_iter().flatten() {
            node(memo, dag, u, depth + 1, seen);
        }
    }
    node(memo, dag, dag.root, 0, &mut HashSet::new());
}
```

For a workload, choose one root candidate per query and initialize the pending queue with the distinct chosen roots. Deduplicate that queue before expansion so a shared root is processed once.

Before accuracy filtering, the running example has two alternatives for A (the two G2 builds) and four for B: G4 and G5 choose independently between the two G2′ builds. Joint enumeration therefore has eight choices. These are structural choices only; constraint (K) decides which meet each target.

| B's p99 producer | B's p50 producer | State sharing within B |
|---|---|---|
| `alpha = 0.01` | `alpha = 0.01` | One logical state candidate |
| `alpha = 0.01` | `alpha = 0.02` | Two state candidates |
| `alpha = 0.02` | `alpha = 0.01` | Two state candidates |
| `alpha = 0.02` | `alpha = 0.02` | One logical state candidate |

The printer marks repeated candidates as references rather than expanding them twice. Physical sharing and cost additionally depend on lifetime and execution context.

For an acyclic candidate-dependency space, a tree-expansion count bounds the number of DAGs:

$$
N(G)=\sum_{v\in G}N(v),\qquad
N(v)=\prod_i\sum_{u\in P^{\mathrm{log,adm}}_{v,i}}N(u).
$$

Here, $P^{\mathrm{log,adm}}_{v,i}$ is the set of logically compatible producers for that input, and a leaf has count one. The bound overcounts shared subgraphs. For a cyclic memo, this recurrence is not well-founded; use cycle-aware enumeration or an explicitly bounded unfolding instead.

For physical inspection, replace logical candidates and compatibility checks with physical candidates and admissible edges. Under the execution-context extension, candidate-level listing alone is not a list of executable plans: instantiate contexts and apply alignment, retention, root-accuracy, and other enabled constraints before comparing results with Stage 3.
