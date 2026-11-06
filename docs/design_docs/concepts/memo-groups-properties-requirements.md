# Memo groups, properties and requirements

This document explains how the optimizer represents alternatives, checks whether they can be composed, and selects an executable plan. It follows the supplied **Optimization Problem Formulation** for the baseline model and labels additional design work **Extension (#1207)**.

The Rust blocks are interface sketches, not standalone compilable implementations. Repository paths and issue references are retained from the supplied draft; their current implementation status has not been independently checked.

## 1. Model and running example

A **workload** contains query expressions, their evaluation schedules, and their accuracy targets. A **memo group** represents one logical result or one intermediate state family over specified input data. A **candidate** is one way to compute that result: one operator over input groups, together with its properties.

Input groups leave producer choices open. Selecting a candidate does not select its entire input plan. Each consumer input chooses a producer separately, and several inputs may choose the same producer.

The expression includes the operator's arguments, including the summary family. In this design, KLL and DDSketch states belong to separate state groups. A final-result group can offer readout candidates that use either family. This resolves the old decision tree's contradiction: it must not put KLL and DDSketch builds in one state group.

### Running example

Query A requests the p95 latency over one minute. Query B requests p99 divided by p50 over the same input. Both run every ten seconds.

| Group | Result | Candidate expressions |
|---|---|---|
| G1 | Raw latency input | `Scan latency[1m]` |
| G2 | KLL state over G1 | `SummaryAgg[KLL](G1)` with `k = 200` or `k = 345` |
| G3 | Query A output | `SummaryEstimate[q=0.95](G2)` |
| G4 | p99 latency | `SummaryEstimate[q=0.99](G2)` |
| G5 | p50 latency | `SummaryEstimate[q=0.50](G2)` |
| G6 | Query B output | `Divide(G4, G5)` |

G3, G4, and G5 choose a G2 producer independently. If all choose the same retained physical producer, they can reuse one computed state when their schedules and windows are compatible. An unmaterialized candidate may instead execute separately in different contexts; Section 5 explains that distinction.

The old example assigned illustrative KLL rank errors of 0.008 and 0.005 and added them to obtain Query B's ratio error. That inference is invalid: rank error does not directly bound numerical quantile error or relative ratio error. The example here demonstrates representation and sharing only. Accepting Query B requires an applicable error model, including assumptions about its denominator.

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

The supplied LaTeX uses the producer-set symbol in two different ways: the prose defines a filtered set, while its notation table defines an unfiltered set. This document follows the table and constraints (G)–(H), and gives the filtered set a separate name:

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
| Parameters | A predefined configuration list per family: KLL and KMV `k`; DDSketch `alpha`; HLL `precision`; CMS and CountSketch `width` and `depth`; heap variants also `heap_size`; UnivMon its sizing parameters. Exact operators have no sketch parameters. | `SketchParams` in `crates/types/src/ir/schema/sketch.rs`; predefined lists are the #1203 target |
| Window layout — Extension | No layout, or tumbling windows with a positive width. Candidate widths come from supported divisors derived from workload ranges and steps. Sliding merge/subtraction layouts remain future work. Window count is derived from range and width; retention amount is selected later. | `WindowLayout`; `pass2/memo/window_candidates.rs`; #1160 |
| Timing | `IngestionTime` or `QueryTime` | `ExecutionTiming` |
| Lifetime | `None`: discard after the consuming execution; `OneEvaluation`: retain until consumers within that evaluation finish; `AcrossEvaluations`: retain across evaluations in the specified period. A non-`None` lifetime means materialized. | `Lifetime`; #1200 / #1214 |
| Medium — Extension | `Memory` or `Disk`, carried by a materialized lifetime | `Medium`; #1214 |
| Schedule — Extension | For `OneEvaluation`, a periodic interval and phase; for windowed `AcrossEvaluations`, a tumbling-window grid phase. Unmaterialized candidates have no independent schedule. | `Schedule`; #1200 PR 2 |

Error guarantees are derived from parameters and the statistic being read. They include an error metric, a bound, and a failure probability; exact computation introduces no approximation error. The same state may support different readouts, so its guarantee must be interpreted for the relevant statistic.

The extended property tuples add window layout and schedule:

$$
p_v^{\mathrm{log}}=(\boldsymbol\theta_v,\omega_v),\qquad
p_v^{\mathrm{phys}}=(\tau_v,\ell_v,\sigma_v).
$$

### Logical requirements

The proposed `LogicalRequirement` stores a **parameter floor**: the weakest configuration in a family's ordered list that satisfies the applicable input error cap. Section 4 defines the cap and distinguishes validity from pruning. It does not allocate a fixed share of the query's error budget to each input.

For KLL, a larger `k` satisfies a smaller minimum `k`. For DDSketch, a smaller `alpha` can be stronger. Multi-parameter families require the family's comparison rule, including parameters needed for failure guarantees. A single floor is valid only if the accepted configurations form an upward-closed set with a representable least element. Otherwise, compatibility must use an explicit admissible set or predicate; it must not invent a unique minimum.

The required input group's identity already constrains the state family and data/window semantics. An exact producer satisfies an approximation floor only if it is also a valid producer of that group and input type.

### Physical requirements

Physical requirements are derived from the consumer's timing and schedule, rather than stored redundantly. For an unmaterialized consumer, the schedule comes from its execution context during selection. Stage 2 checks the parts known from candidate properties; Stage 3 checks any remaining context-dependent delivery condition.

| Consumer timing | QT, None | QT, OneEvaluation | QT, AcrossEvaluations | IT, None | IT, AcrossEvaluations |
|---|---|---|---|---|---|
| QueryTime | Allowed | Allowed | Allowed | Rejected | Allowed |
| IngestionTime | Rejected | Rejected | Rejected | Allowed | Allowed |

QT means QueryTime; IT means IngestionTime. Schedule, data coverage, and retention checks still apply to allowed combinations.

**Periodic versus grid.** A periodic schedule with interval ten seconds and phase two produces at 2, 12, 22, … seconds. A five-second tumbling grid with phase zero partitions the input into windows [0, 5), [5, 10), … . The first describes evaluation times; the second describes window boundaries. Grid alignment alone does not ensure sufficient retained history.

For the proposed periodic consumer path, let the consumer interval and phase be $I$ and $\varphi$. Let the producer interval and phase be $I_s$ and $\varphi_s$, and its tumbling width and grid phase be $w$ and $\varphi_g$.

| Producer schedule | Required alignment |
|---|---|
| Periodic | $I_s\mid I$ and $\varphi\equiv\varphi_s\pmod{I_s}$ |
| Tumbling grid | $w\mid I$ and $\varphi\equiv\varphi_g\pmod w$ |
| None | Runs in the consuming context; no independent delivery schedule |

The supplied formulation also permits a single, non-periodic evaluation. The interval/phase interface below covers only periodic consumers. Rejecting non-periodic consumers is a current scope restriction of that proposed interface, not a mathematical impossibility; supporting them needs explicit evaluation-time membership checks.

### Invalid combinations

Checked constructors reject these combinations under the proposed schedule model:

| Combination | Reason |
|---|---|
| IngestionTime with OneEvaluation | No query evaluation lifetime at ingestion time |
| None with an independent schedule | Execution follows the consuming context |
| Materialized lifetime without its required schedule | Delivery times or window grid would be unspecified |
| OneEvaluation with a grid, or windowed AcrossEvaluations with a periodic schedule | Schedule kind does not match this model's lifetime representation |
| Zero interval/width, or phase outside the canonical interval/width | Invalid schedule representation |
| Ingestion-time grid with nonzero phase | Unsupported by the draft's epoch-anchored ingestion representation |

A tumbling grid also needs a known positive width. The `PhysicalProperties` constructor alone cannot validate a width held in logical properties; physical candidate construction must validate the pair. Non-windowed AcrossEvaluations schedules need an explicit representation before that alternative can be offered.

## 3. Pipeline: interfaces and rules

| Stage | Input | Output | Responsibility |
|---|---|---|---|
| Stage 0 | Query IR | `DagOfLogicalMemoGroups` | Hash-cons logical expressions |
| Stage 1, Pass 1 | Group DAG | `Memo` | Add initial logical candidates and intermediate groups |
| Stage 1, Pass 2 | Memo | `LogicalASAPMemoCandidates` | Add reuse alternatives and compute logical edges |
| Stage 2 | Logical memo candidates | `PhysicalASAPMemoCandidates` | Expand physical alternatives and compute compatibility |
| Stage 3 | Physical memo candidates | `DagOfSelectedCandidates` | Select implementations, dependencies, and executions |
| Realizer/export | Selected DAG | `PhysicalASAPDAG` | Build and export the executable graph |

Enumeration proceeds through expression, parameters, window layout, timing, lifetime, and schedule. Executor and deployment restrictions prune unsupported alternatives as soon as their prerequisites are known. Accuracy pruning is allowed only under the conditions in Section 4. Reordering dimensions preserves the candidate set only when the same alternatives and sound pruning predicates are retained.

Rules add groups through `find_or_add_group` and candidates through `add_candidate`. They match group expressions, avoid duplicating an existing expression/property pair, and do not copy consumers to make a new producer visible. After logical generation finishes, edge construction records the possible producer choices.

### Stage 0: groups

`hash_consed` in `crates/logical-asap-optimization/src/stage0.rs` identifies shared logical expressions and query roots. The following preserves the existing group representation described in the draft. Candidate ownership is exposed through the group in the proposed Stage 1 interface.

```rust
pub struct MemoGroupId(pub u32);

pub struct MemoGroup {
    pub logical_node: Rc<OperatorNode>,
    pub child_groups: Vec<MemoGroupId>,
    pub parents: Vec<MemoGroupId>,
    pub queries: Vec<usize>,
    // Empty after Stage 0; populated by Stage 1.
    pub candidates: Vec<CandidateForMemoGroup>,
}

pub struct DagOfLogicalMemoGroups {
    pub groups: Vec<MemoGroup>,
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
}
```

`OperatorNode` recursively contains chosen child nodes and therefore describes a fixed plan fragment. `Operator<MemoGroupId>` contains input group IDs and leaves their implementations open. A candidate needs its own expression because a group may offer several ways to compute the same result; it cannot reuse the group's representative `logical_node` as every candidate's expression.

Replacing the group's representative node and child list with a single group-based expression, or removing the one-field `CandidateExpression` wrapper, remains an optional simplification from the review. Neither is required here.

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
    pub index: usize,
}

pub struct Memo {
    pub groups: Vec<MemoGroup>,
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
}

impl Memo {
    pub fn find_or_add_group(
        &mut self,
        expression: Operator<MemoGroupId>,
    ) -> MemoGroupId;

    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> CandidateForMemoGroupId;

    pub fn candidate(&self, id: CandidateForMemoGroupId) -> &CandidateForMemoGroup;
}

pub struct CandidateExpression {
    pub operator: Operator<MemoGroupId>,
}

pub struct CandidateForMemoGroup {
    pub expression: CandidateExpression,
    pub logical_properties: LogicalProperties,
    pub input_requirements: Vec<LogicalRequirement>,
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
    pub fn new(
        expression: &CandidateExpression,
        parameters: Option<SketchParams>,
        window_layout: Option<WindowLayout>,
    ) -> Result<Self, InvalidLogicalProperties>;

    pub fn parameters(&self) -> Option<&SketchParams>;
    pub fn window_layout(&self) -> Option<&WindowLayout>;
}

pub fn parameter_list(family: SketchAlgorithm) -> &'static [SketchParams];

pub struct LogicalRequirement {
    parameter_floor: Option<SketchParams>,
}

impl LogicalRequirement {
    pub fn new(
        parameter_floor: Option<SketchParams>,
    ) -> Result<Self, InvalidLogicalProperties>;

    pub fn parameter_floor(&self) -> Option<&SketchParams>;
}

pub fn satisfies_logical(
    producer: &LogicalProperties,
    requirement: &LogicalRequirement,
) -> bool;
```

The floor representation has the applicability condition stated in Section 2. Creating requirements also needs the input family's configuration domain; a constructor without that context can validate only the information encoded in its argument. Error-model interfaces are grouped with accuracy in Section 4.

Migration targets: remove private `root_node` chains, duplicate `input_groups`, and `accuracy_budget_parts`. The proposed predefined parameter lists replace sizing one candidate per budget partition.

### Stage 1, Pass 2: reuse rules and logical edges

The following preserves the supplied rule inventory and tracking references. Paths are relative to `crates/logical-asap-optimization/src/pass2/memo/`.

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

After the rules, build logical edges and apply any proven-safe accuracy pruning. `input_index_of_consumer` replaces the ambiguous `input` field name throughout this document's edge interfaces.

```rust
fn apply(memo: &mut Memo) -> Result<(), Pass2RuleError>;

pub struct LogicalEdge {
    pub producer: CandidateForMemoGroupId,
    pub consumer: CandidateForMemoGroupId,
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

pub enum Medium {
    Memory,
    Disk,
}

pub enum Lifetime {
    None,
    OneEvaluation(Medium),
    AcrossEvaluations(Medium),
}
pub enum Schedule {
    Periodic { interval_ms: u64, phase_ms: u64 },
    TumblingWindowGrid { phase_ms: u64 },
}
pub struct PhysicalProperties {
    timing: ExecutionTiming,
    lifetime: Lifetime,
    schedule: Option<Schedule>,
}
impl PhysicalProperties {
    pub fn new(
        timing: ExecutionTiming,
        lifetime: Lifetime,
        schedule: Option<Schedule>,
    ) -> Result<Self, InvalidPhysicalProperties>;
    pub fn timing(&self) -> ExecutionTiming;
    pub fn lifetime(&self) -> Lifetime;
    pub fn schedule(&self) -> Option<&Schedule>;
    pub fn is_materialized(&self) -> bool;
}

pub struct EvaluationSchedule {
    interval_ms: u64,
    phase_ms: u64,
}
impl PhysicalProperties {
    pub fn delivers_on(&self, times: &EvaluationSchedule) -> bool;
}
pub fn satisfies_physical(
    producer: &PhysicalProperties,
    consumer: &PhysicalProperties,
) -> bool;
pub struct PhysicalCandidateId(pub usize);
pub struct PhysicalEdgeId(pub usize);

pub struct PhysicalCandidate {
    pub logical: CandidateForMemoGroupId,
    pub properties: PhysicalProperties,
}
pub struct PhysicalEdge {
    pub producer: PhysicalCandidateId,
    pub consumer: PhysicalCandidateId,
    pub input_index_of_consumer: usize,
    pub satisfies_logical_requirement: bool,
    pub satisfies_physical_requirement: bool,
}
pub struct PhysicalASAPMemoCandidates {
    pub logical: LogicalASAPMemoCandidates,
    pub candidates: Vec<PhysicalCandidate>,
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
    pub logical: CandidateForMemoGroupId,
    pub properties: PhysicalProperties,
}
```

`PhysicalCandidateId` indexes an emitted candidate. `PhysicalCandidateKey` describes an alternative rejected before it received an ID, allowing diagnostics to explain pruning. The admissible-edge accessor returns edge IDs, not the unfiltered producer set from Section 1.

The `delivers_on` sketch needs access to a tumbling producer's logical window width. Supply that context in its implementation or move the check to the full physical-candidate interface; the phase-only schedule is insufficient by itself.

Migration targets include replacing `Materialization` and duplicated physical fields with checked properties, and replacing `ReaderConstraint::{IncompatibleOutputs, AllowedOutputs}` with compatibility and schedule checks. Policy constraints involving the number of selected consuming queries are a separate concern; see Section 5.

### Stage 3: baseline selection and realization

The baseline uses the variables and constraints in the supplied LaTeX. Stage 3 adds no candidates or potential dependencies.

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

The realizer uses the selected dependencies to build the executable graph. Export carries each node's timing, lifetime, and supported schedule fields. Under the context extension, multiple selected execution nodes may refer to the same physical candidate; realization must preserve their contexts rather than merge them solely by candidate ID.

## 4. Accuracy and safe pruning — Extension (#1207)

The supplied LaTeX checks logical requirements per edge. The proposed extension additionally checks composed accuracy at each query root. This section describes the error model needed for that check; it is not an additional property dimension.

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

A filter does not automatically preserve every accuracy guarantee. Group counts and metric conversions may need metadata or assumptions beyond the expression syntax. Propagation factors must be justified by the relevant operator/error contract.

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

The draft's approved interface is retained here as an interface proposal:

```rust
pub struct ErrorPropagation {
    pub factor_per_input: Vec<f64>,
    pub validity_cap: Option<f64>,
}

impl LogicalRequirement {
    pub fn error_propagation(
        consumer: &Operator<MemoGroupId>,
    ) -> ErrorPropagation;
}
```

`factor_per_input` contains the local beta factors. A scalar `validity_cap` can represent a conservative common cap. Operator-specific caps, such as a bound only on a ratio's denominator, need either a documented input mapping or a per-input representation before implementation. Unsupported error models must be reported as unsupported rather than supplied with guessed factors.

### Query composition and root checks

An **error source** is one local approximation event with a stated metric and guarantee. Let $s$ index such sources in a selected, acyclic plan. Let $\varepsilon_s$ and $\delta_s$ be its bound and failure probability. Define $\lambda_{q,s}$ as the sum, over paths from that source to the query output, of products of local beta factors. Define $a_{q,s}$ as a binary indicator that query q's selected dependency graph uses that source.

The intended root conditions are:

$$
\sum_s\lambda_{q,s}\varepsilon_s a_{q,s}\le\varepsilon_q,
\qquad
\sum_s\delta_s a_{q,s}\le\delta_q.
$$

Here, $\varepsilon_q$ and $\delta_q$ are the query target's error and failure limits. Repeated paths can amplify error and therefore contribute to lambda. A shared failure event is counted once in the union bound; distinct readout events need distinct source identities unless a joint guarantee is available.

These equations describe the required bound. They are a linear MILP encoding only when the factors and source identities are fixed for the indexed alternatives and the reachability indicators are correctly linked to selected edges and outputs. Candidate-dependent paths, metric changes, or execution-dependent random events require a more detailed encoding. The draft does not supply that encoding, so the equations must not be described as a completed constraint implementation.

This replaces the old unexplained `a_{q,u}` and the mixture of candidate-, edge-, and query-indexed lambda symbols. Local propagation uses beta; query propagation uses lambda; neither should be inferred from bare edge selection alone.

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

These extensions go beyond constraints (A)–(I) and the fixed-cost assumption in the supplied LaTeX. They are separated here so that the baseline remains understandable without an unfinished context MILP.

### Execution identity

A materialized candidate has its own delivery schedule. An unmaterialized candidate runs inside an execution that needs it. For example, a finalize candidate used by queries A and B may execute in both query contexts even though Stage 2 enumerates it only once.

A **context** identifies a class of executions with a common owner and schedule across the accounting horizon. It is not necessarily one timestamp.

```rust
pub struct ContextId(pub usize);

pub enum Context {
    QueryEvaluation(QueryId),
    IngestionPipeline(RefreshWindowId),
    Materialized(PhysicalCandidateId),
}

impl Context {
    // Periodic schedule when supported; None is not an implicit compatibility pass.
    pub fn evaluation_schedule(&self) -> Option<EvaluationSchedule>;
}
```

An unmaterialized chain inherits its consuming context. Encountering a materialized producer switches to that producer's own computation context when traversing its inputs. The context identifies where the operation runs; schedule compatibility decides whether a materialized input can supply it.

| Extended variable | Meaning |
|---|---|
| $x_v^{\mathrm{mat}}$ | Materialized candidate selected |
| $x_{\kappa,v}$ | Unmaterialized candidate executes in context kappa |
| $y_{\kappa,u,c,i}$ | Input edge selected for a consumer execution in context kappa |
| $h_v$ | Selected retention amount for a materialized producer |

The unindexed edge variable belongs to the baseline only. In the extension, every edge is indexed by its consumer's context, including the dedicated computation context of a materialized consumer. This avoids silently switching between two meanings of `y`.

Activation, input satisfaction, utilization, output selection, and acyclicity must be defined for these execution instances. One binary per context/candidate assumes at most one reusable execution of that candidate within that context. If two uses require independent computations, the context or execution identity must distinguish them.

### Cost and retention

To avoid confusing per-execution costs with the baseline's horizon-wide cost, use separate coefficient names:

| Symbol | Definition |
|---|---|
| $W_v^{\mathrm{mat}}$ | Fixed computation and access cost over the horizon for a materialized candidate's own schedule, excluding variable retention |
| $c_{\kappa,v}$ | Cost of one unmaterialized execution in the specified context |
| $\rho_\kappa$ | Fixed number or expected number of context executions over the common horizon |
| $h_v$ | Retention amount in a declared unit, such as retained windows |
| $\pi_v$ | Horizon-wide retention cost per unit of retained amount |
| $\operatorname{need}_{\kappa,v,c,i}$ | Retention amount producer v needs to serve this consumer input in this context |

A proposed linear objective is:

$$
\min\left(
\sum_{v\text{ materialized}}W_v^{\mathrm{mat}}x_v^{\mathrm{mat}}
+\sum_{\kappa,v\text{ unmaterialized}}\rho_\kappa c_{\kappa,v}x_{\kappa,v}
+\sum_{v\text{ materialized}}\pi_v h_v
\right).
$$

A selected input edge imposes the corresponding retention lower bound:

$$
h_v\ge\operatorname{need}_{\kappa,v,c,i}\,y_{\kappa,v,c,i}.
$$

The retention domain, upper bounds, activation linking, and all coefficient units must be specified by the implementation. If required retention depends on selected downstream queries, that dependence needs explicit linking constraints; it is not a fixed coefficient merely because it has a name.

### Extension status and legacy policy names

| Earlier label | Meaning and status |
|---|---|
| (J) | Placeholder for consumer/reachability policies; absent from the supplied LaTeX and not defined by this document |
| `MinimumQueryReaders` | Legacy policy name suggesting a minimum number of distinct consuming queries. Its threshold, counting scope, and activation rule are unspecified in the supplied material; no constraint is asserted here. |
| `HighestSharedQueryMaterialization` | Legacy policy name with no supplied mathematical definition; retained as a migration question, not a defined constraint |
| (K) | Proposed root-accuracy check; Section 4 states the semantic bound and encoding conditions |
| (R) | Proposed retention constraints; lower-bound pattern above |
| (L) | Proposed per-query latency bounds; requires an execution/critical-path model |
| Resource budgets | Proposed memory and CPU limits; requires explicit accounting units and aggregation rules |

These labels must not be presented as an implemented sequence of constraints (A)–(K). The review text contains no definitions that would justify filling the gaps by assumption.

## Appendix A. Enumerating DAGs for debugging

This is an inspection tool for small memos, not a planning stage. The MILP selects a plan without listing all alternatives.

A logical DAG has one selected root candidate, one compatible producer per input of every reached candidate, no unused nodes, and no cycles. Producer choices are per consumer input, not per group. Two consumers can choose different candidates of the same group; choosing the same candidate shares one logical node.

The following Rust-style pseudocode preserves the draft's enumeration procedure. Helpers such as `cartesian_product` are schematic; `reaches` follows the stored consumer-to-producer dependency direction and is reflexive. This direction makes the cycle test precise.

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
                satisfies_logical(
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
    (0..memo.groups[group.0 as usize].candidates.len())
        .map(move |index| CandidateForMemoGroupId { group, index })
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
            println!("{}↑ G{}[{}] (shared)", "  ".repeat(depth), c.group.0, c.index);
            return;
        }
        println!("{}G{}[{}] {} {:?}", "  ".repeat(depth), c.group.0, c.index,
                 candidate.expression.operator.name(), candidate.logical_properties);
        for &u in dag.producers.get(&c).into_iter().flatten() {
            node(memo, dag, u, depth + 1, seen);
        }
    }
    node(memo, dag, dag.root, 0, &mut HashSet::new());
}
```

For a workload, choose one root candidate per query and initialize the pending queue with the distinct chosen roots. Deduplicate that queue before expansion so a shared root is processed once.

Before accuracy filtering, the running example has two alternatives for A and four for B: G4 and G5 choose independently between the two G2 builds. Joint enumeration therefore has eight choices. These are structural choices only; no claim is made that they meet B's ratio-accuracy requirement.

| B's p99 producer | B's p50 producer | State sharing within B |
|---|---|---|
| `k = 200` | `k = 200` | One logical state candidate |
| `k = 200` | `k = 345` | Two state candidates |
| `k = 345` | `k = 200` | Two state candidates |
| `k = 345` | `k = 345` | One logical state candidate |

The printer marks repeated candidates as references rather than expanding them twice. Physical sharing and cost additionally depend on lifetime and execution context.

For an acyclic candidate-dependency space, a tree-expansion count bounds the number of DAGs:

$$
N(G)=\sum_{v\in G}N(v),\qquad
N(v)=\prod_i\sum_{u\in P^{\mathrm{log,adm}}_{v,i}}N(u).
$$

Here, $P^{\mathrm{log,adm}}_{v,i}$ is the set of logically compatible producers for that input, and a leaf has count one. The bound overcounts shared subgraphs. For a cyclic memo, this recurrence is not well-founded; use cycle-aware enumeration or an explicitly bounded unfolding instead.

For physical inspection, replace logical candidates and compatibility checks with physical candidates and admissible edges. Under the execution-context extension, candidate-level listing alone is not a list of executable plans: instantiate contexts and apply schedule, retention, root-accuracy, and other enabled constraints before comparing results with Stage 3.
