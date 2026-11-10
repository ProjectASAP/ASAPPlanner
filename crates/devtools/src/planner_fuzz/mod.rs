//! `planner_fuzz`: a metamorphic fuzz tester for missed reuse and sharing.
//!
//! A missed-reuse bug is a plan the planner's rules never generate: the
//! answers stay correct, and an exhaustive search over the generated
//! candidates cannot see it. The tester finds one by relating two
//! workloads. For a query `q` of a workload `W` and a query `q'` derivable
//! from `q`'s result or state (a filter on a group key, a coarser
//! grouping, ...), it builds a
//! *witness*: `W`'s selected logical plan plus `q'` derived from `q`'s
//! node by one operator. The witness is costed by `asap_cost::plan_cost`,
//! as Stage 3 costs its plan: each node of `W`'s plan keeps the timing,
//! lifetime, θ, coverage and retention Stage 3 gave it, and each derived
//! node runs at query time, not materialized (the witness holds ASAP
//! operators, which Pass 1 does not take as input, so it is not planned
//! again). The relation
//!
//! ```text
//! cost(W ∪ {q'}) − cost(W) ≤ derive_cost + tolerance,
//! derive_cost = cost(witness) − cost(W's plan costed the same way)
//! ```
//!
//! must hold. `derive_cost` is what the cost model charges for the derived
//! operator alone (its per-node cost, plus any change it causes in the
//! shared nodes' costs, such as a shared node now materialized for one
//! evaluation); it is never a guess. The tester plans with accuracy not
//! enforced (every query without an accuracy target), so a witness needs
//! no accuracy check. A violation means a cheaper plan for `W ∪ {q'}` exists
//! that the planner did not select. Monotonicity and invariance relations
//! (tightening accuracy, adding a query or dropping raw retention never
//! lowers cost; relaxing a latency bound never raises it; renaming,
//! reordering and disjoint copies do not change it) catch the same kind of
//! miss without a witness.
//!
//! [`relations`] defines the relations, [`generate`] the random workloads,
//! [`shrink`] the greedy shrinker and [`report`] the summary and the
//! issue drafts.

pub mod generate;
pub mod relations;
pub mod report;
pub mod shrink;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use asap_cost::{plan_cost, CostInputs};
use asap_planner::PlanSearchSettings;
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::flat::flatten;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId};
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::{AccuracyTarget, ExecutionTiming};
use asap_types::ir::schema::sketch::SketchAlgorithm;
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot};
use asap_types::physical::{Lifetime, StorageMedium};
use asap_types::workload::{
    DataWorkload, PlanningWorkload, QueryLanguage, QueryWorkload, QueryWorkloadEntry,
    RepeatingEntry, SqlDialect,
};

use crate::corpus_registry::arrival_from_sources;
use crate::planning_stages::{lower_workload, planning_models, run_search_over, sql_catalog};
use generate::QuerySpec;

/// The language of a fuzz workload: one per workload, as one
/// `asap_planner --workload` file has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Promql,
    Sql,
}

/// One repeating query: its workload entry, and its structured form when
/// the generator made it (`None` for a query read from a seed file, which
/// only the relations that need no structure apply to).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuzzQuery {
    pub spec: Option<QuerySpec>,
    pub entry: RepeatingEntry,
}

impl FuzzQuery {
    /// A generated query evaluated every `repeat_interval_ms`, predictable.
    pub fn generated(
        spec: QuerySpec,
        accuracy: AccuracyTarget,
        repeat_interval_ms: u64,
        max_latency_ms: Option<f64>,
    ) -> Self {
        let entry = serde_json::from_value(json!({
            "query": spec.render(),
            "demand": {"fixed_interval_at": {"interval": repeat_interval_ms, "evaluation_phase": 0}},
            "requirements": {"accuracy": accuracy, "max_latency_ms": max_latency_ms},
            "predictability": {"predictable": {"known_at": null}},
        }))
        .expect("a generated entry deserializes");
        Self {
            spec: Some(spec),
            entry,
        }
    }

    pub fn text(&self) -> &str {
        &self.entry.query.0
    }

    pub fn accuracy(&self) -> &AccuracyTarget {
        &self.entry.requirements.accuracy
    }

    /// This query with `spec` in place of its own, its text rendered again.
    pub fn with_spec(&self, spec: QuerySpec) -> Self {
        let mut query = self.clone();
        query.entry.query.0 = spec.render();
        query.spec = Some(spec);
        query
    }

    pub fn with_accuracy(&self, accuracy: AccuracyTarget) -> Self {
        let mut query = self.clone();
        query.entry.requirements.accuracy = accuracy;
        query
    }
}

/// A workload the tester plans: queries in one language, the data facts,
/// the SQL tables they read (`asap_planner --sql-catalog-table` objects) and whether
/// the deployment keeps raw data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuzzWorkload {
    pub language: Language,
    pub queries: Vec<FuzzQuery>,
    pub data: DataWorkload,
    pub tables: Vec<Value>,
    pub raw_data_retained: bool,
    /// `DeploymentCapabilities::query_time_results_retained` (default
    /// `false`).
    #[serde(default)]
    pub query_time_results_retained: bool,
    /// The only sketch families the deployment builds; `None` for every
    /// family the executor builds. A witness plans `W` with one family to
    /// have the planner size that family's build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sketch_families: Option<Vec<SketchAlgorithm>>,
}

impl FuzzWorkload {
    /// The `PlanningWorkload` (`asap_planner --workload` file) of these
    /// queries.
    pub fn planning_workload(&self) -> PlanningWorkload {
        PlanningWorkload {
            query_workload: QueryWorkload {
                language: match self.language {
                    Language::Promql => QueryLanguage::PromQL,
                    Language::Sql => QueryLanguage::SQL(SqlDialect::DataFusionSQL),
                },
                query_batch: None,
                repeating_queries: Some(self.queries.iter().map(|q| q.entry.clone()).collect()),
            },
            data_workload: Some(self.data.clone()),
        }
    }

    /// Sets the ingesting arrival rate to the sum of the sources' own rates
    /// ([`arrival_from_sources`]), when every source declares one.
    pub fn set_arrival_from_sources(&mut self) {
        if let Some(arrival) = arrival_from_sources(&self.data) {
            self.data.arrival = arrival;
        }
    }

    pub fn with_query(&self, query: FuzzQuery) -> Self {
        let mut workload = self.clone();
        workload.queries.push(query);
        workload
    }

    pub fn without_query(&self, index: usize) -> Self {
        let mut workload = self.clone();
        workload.queries.remove(index);
        workload
    }

    pub fn with_query_replaced(&self, index: usize, query: FuzzQuery) -> Self {
        let mut workload = self.clone();
        workload.queries[index] = query;
        workload
    }

    /// Whether every query has a structured form.
    pub fn all_structured(&self) -> bool {
        self.queries.iter().all(|q| q.spec.is_some())
    }

    /// The `--deployment-capabilities` this workload plans with.
    pub fn deployment(&self) -> DeploymentCapabilities {
        DeploymentCapabilities {
            supported_summaries: Some(
                asap_executor::supported_summary_families()
                    .into_iter()
                    .filter(|supported| {
                        self.sketch_families
                            .as_ref()
                            .is_none_or(|only| only.contains(&supported.summary_type))
                    })
                    .collect(),
            ),
            raw_data_retained: self.raw_data_retained,
            query_time_results_retained: self.query_time_results_retained,
        }
    }

    /// The `asap_planner` invocation (a bash command) that plans this
    /// workload, saved as `workload_file`, with accuracy not enforced, as
    /// the tester plans. Its output document is `<workload_file stem>.out.json`.
    pub fn asap_planner_command(&self, workload_file: &str) -> String {
        let mut command = String::new();
        command.push_str(&format!(
            "cargo run -p asap-devtools --bin asap_planner -- --workload {workload_file} \
             --run-until stage3 --format json"
        ));
        for table in &self.tables {
            command.push_str(&format!(" \\\n  --sql-catalog-table '{table}'"));
        }
        command.push_str(" --ignore-accuracy-guarantees");
        if !self.raw_data_retained || self.query_time_results_retained {
            command.push_str(&format!(
                " --deployment-capabilities <(echo '{{\"raw_data_retained\": {}, \"query_time_results_retained\": {}}}')",
                self.raw_data_retained, self.query_time_results_retained
            ));
        }
        let stem = workload_file.strip_suffix(".json").unwrap_or(workload_file);
        command.push_str(&format!(" --out {stem}.out.json"));
        command
    }
}

/// Workloads read from a seed file: a `PlanningWorkload` JSON (its
/// repeating queries), with the SQL tables of a `tables.json` (a JSON array
/// of `--sql-catalog-table` objects) in the same directory, or an object
/// `{"workload": <PlanningWorkload>, "tables": [...]}`.
pub fn read_seed_file(path: &std::path::Path) -> Result<FuzzWorkload, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let (workload, tables) = match value.get("workload") {
        Some(workload) => (
            workload.clone(),
            value.get("tables").cloned().unwrap_or_else(|| json!([])),
        ),
        None => {
            let tables_path = path.with_file_name("tables.json");
            let tables = match std::fs::read_to_string(&tables_path) {
                Ok(text) => serde_json::from_str(&text)
                    .map_err(|e| format!("{}: {e}", tables_path.display()))?,
                Err(_) => json!([]),
            };
            (value, tables)
        }
    };
    let workload: PlanningWorkload =
        serde_json::from_value(workload).map_err(|e| format!("{}: {e}", path.display()))?;
    let language = match workload.query_workload.language {
        QueryLanguage::PromQL => Language::Promql,
        QueryLanguage::SQL(_) => Language::Sql,
        ref other => {
            return Err(format!(
                "{}: unsupported language {other:?}",
                path.display()
            ))
        }
    };
    let queries: Vec<FuzzQuery> = workload
        .query_workload
        .repeating_queries
        .unwrap_or_default()
        .into_iter()
        .map(|entry| FuzzQuery { spec: None, entry })
        .collect();
    if queries.is_empty() {
        return Err(format!("{}: no repeating queries", path.display()));
    }
    Ok(FuzzWorkload {
        language,
        queries,
        data: workload.data_workload.unwrap_or_default(),
        tables: tables.as_array().cloned().unwrap_or_default(),
        raw_data_retained: true,
        query_time_results_retained: false,
        sketch_families: None,
    })
}

/// Which search plans each workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchKind {
    /// Joint candidate-connection planning, the facade's search.
    CandidateConnections,
}

/// How the tester plans and judges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuzzSettings {
    pub search: SearchKind,
    /// A relation is violated when it misses by more than
    /// `max(relative_tolerance × cost, absolute_tolerance)`, `cost` the
    /// larger side's cost per second.
    pub relative_tolerance: f64,
    pub absolute_tolerance: f64,
}

impl Default for FuzzSettings {
    fn default() -> Self {
        Self {
            search: SearchKind::CandidateConnections,
            relative_tolerance: 0.01,
            absolute_tolerance: 1e-6,
        }
    }
}

impl FuzzSettings {
    pub fn tolerance(&self, cost_per_second: f64) -> f64 {
        (self.relative_tolerance * cost_per_second.abs()).max(self.absolute_tolerance)
    }
}

/// One workload planned by the planner's search.
#[derive(Debug)]
pub struct Planned {
    /// The selected plan, labelled for reports ([`plan_label`]).
    pub selected: String,
    /// Its cost per second under cost model 1.
    pub cost_per_second: f64,
    /// The frontends' roots, one per query.
    pub stage0_roots: Vec<QueryRoot>,
    /// The plan Stage 3 selected.
    pub plan: PhysicalASAPDAG,
    /// `plan`'s query roots as operator trees, one per query, a node the
    /// plan shares shared: the roots a witness extends.
    pub roots: Vec<QueryRoot>,
    /// The plan node of each node of `roots`, by the node's address
    /// (`roots` holds the nodes, so no address is reused).
    node_of: HashMap<*const OperatorNode, PhysicalASAPNodeId>,
}

/// A plan costed outside the search ([`Planner::cost`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Costed {
    /// The plan costed, labelled for reports ([`plan_label`]).
    pub selected: String,
    pub cost_per_second: f64,
    /// The nodes only the last root reads: in a witness, the operators
    /// that derive `q'`.
    pub derived_root: PrivateNodes,
}

/// The nodes of a plan only one root reads.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrivateNodes {
    /// Their summed per-node cost per second.
    pub cost_per_second: f64,
    /// Their operators.
    pub operators: Vec<String>,
}

/// `plan`'s query roots as operator trees (each plan node one shared
/// node) and the plan node of each.
fn operator_trees(
    plan: &PhysicalASAPDAG,
) -> (
    Vec<QueryRoot>,
    HashMap<*const OperatorNode, PhysicalASAPNodeId>,
) {
    // Stage 3 numbers producers first.
    let mut tree_of: Vec<Rc<OperatorNode>> = Vec::with_capacity(plan.nodes.len());
    for node in &plan.nodes {
        let operator = node
            .payload
            .map_children(|&child| Rc::clone(&tree_of[child]));
        tree_of.push(Rc::new(
            OperatorNode::with_schema(operator, node.output_schema.clone())
                .with_guarantee(node.guarantee.clone()),
        ));
    }
    let node_of = tree_of
        .iter()
        .enumerate()
        .map(|(id, tree)| (Rc::as_ptr(tree), id))
        .collect();
    let mut operator_roots = plan.roots.iter();
    let roots = (0..plan.roots.len() + plan.scalar_roots.len())
        .map(
            |query| match plan.scalar_roots.iter().find(|(index, _)| *index == query) {
                Some((_, expression)) => QueryRoot::Scalar(
                    expression.map_operator_refs(&mut |&id| Rc::clone(&tree_of[id])),
                ),
                None => QueryRoot::Operator(Rc::clone(
                    &tree_of[*operator_roots.next().expect("one operator root per query")],
                )),
            },
        )
        .collect();
    (roots, node_of)
}

/// `plan`'s nodes in node order, each its operator, marked when it runs at
/// ingestion time or is materialized across evaluations: e.g. `Scan
/// (ingestion), SummaryTumblingWindowChain ExactAggregate(Sum)
/// tumbling_window_width_ms=60000 (ingestion, materialized), TimeRange
/// 1800s, ...`.
fn plan_label(plan: &PhysicalASAPDAG) -> String {
    plan.nodes
        .iter()
        .map(|node| {
            let mut marks = Vec::new();
            if node.output_state.timing == ExecutionTiming::IngestionTime {
                marks.push("ingestion");
            }
            if matches!(node.lifetime, Lifetime::AcrossEvaluations(_)) {
                marks.push("materialized");
            }
            let label = crate::candidate_labels::operator_label(&node.payload);
            match marks.is_empty() {
                true => label,
                false => format!("{label} ({})", marks.join(", ")),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The grid of Stage 3's evaluation context of `entry`'s query: its
/// evaluation grid, or 1 ms when its evaluation times are not known.
fn evaluation_grid_ms(entry: &QueryWorkloadEntry) -> u64 {
    entry
        .recurrence
        .evaluation_grid_ms(entry.range_evaluation)
        .unwrap_or(1)
}

/// Plans and costs workloads, caching each workload's plan and the
/// reference cost of its selected plan.
pub struct Planner {
    pub settings: FuzzSettings,
    cache: HashMap<String, Result<Rc<Planned>, String>>,
    reference_costs: HashMap<String, Result<Costed, String>>,
}

impl Planner {
    pub fn new(settings: FuzzSettings) -> Self {
        Self {
            settings,
            cache: HashMap::new(),
            reference_costs: HashMap::new(),
        }
    }

    fn cost_inputs(&self) -> CostInputs {
        CostInputs::default()
    }

    /// The frontends' roots of `workload`, one per query.
    pub fn lower(&self, workload: &FuzzWorkload) -> Result<Vec<QueryRoot>, String> {
        let tables: Vec<String> = workload.tables.iter().map(Value::to_string).collect();
        lower_workload(&workload.planning_workload(), &sql_catalog(&tables)?.0)
    }

    /// `workload` planned by the configured search, with accuracy not
    /// enforced.
    pub fn plan(&mut self, workload: &FuzzWorkload) -> Result<Rc<Planned>, String> {
        let key = serde_json::to_string(workload).expect("a workload serializes");
        if let Some(planned) = self.cache.get(&key) {
            return planned.clone();
        }
        let planned = self.plan_uncached(workload).map(Rc::new);
        self.cache.insert(key, planned.clone());
        planned
    }

    fn plan_uncached(&self, workload: &FuzzWorkload) -> Result<Planned, String> {
        let roots = self.lower(workload)?;
        let planning = workload.planning_workload();
        let entries: Vec<_> = planning.query_workload.entries().collect();
        let data = workload.data.clone();
        let cost_inputs = self.cost_inputs();
        let deployment = workload.deployment();
        let models = planning_models(&deployment, &cost_inputs, true);
        let run = match self.settings.search {
            SearchKind::CandidateConnections => run_search_over(
                roots.clone(),
                &entries,
                &data,
                models,
                PlanSearchSettings::default(),
            ),
        }
        .map_err(|e| e.to_string())?;
        let (plan, _) = run.run.selection.map_err(|e| format!("selection: {e}"))?;
        let (cost, _) = plan_cost(&plan, &entries, &data, &cost_inputs)
            .map_err(|(node, e)| format!("plan cost, node {node}: {e}"))?;
        let cost_per_second = cost
            .cost_per_second
            .ok_or("no cost per second (cost model 3)")?;
        let (plan_roots, node_of) = operator_trees(&plan);
        Ok(Planned {
            selected: plan_label(&plan),
            cost_per_second,
            stage0_roots: roots,
            plan,
            roots: plan_roots,
            node_of,
        })
    }

    /// The cost of the plan `roots` of `workload`'s queries (one root per
    /// query, in order) under the same cost inputs as [`plan`](Self::plan),
    /// by `asap_cost::plan_cost`: each node of `planned`'s plan keeps the
    /// timing, lifetime, θ, coverage and retention Stage 3 gave it, and
    /// every other node (a witness's derived operators) runs at query time,
    /// not materialized. A `TimeRange` derived over a node that is retained
    /// widens its retention to its range. With `last_root_is_derived` (a
    /// witness, whose last root is `q'`), a query-time node that is not
    /// materialized and is read by the last root and by another root in the
    /// same evaluation context (the same evaluation grid, as Stage 3's
    /// contexts) is materialized for one evaluation instead
    /// (`OneEvaluation(Memory)`): built once per evaluation and read by
    /// both, as Stage 3 may select. `derived_root` is the nodes only the
    /// last root reads.
    pub fn cost(
        &self,
        workload: &FuzzWorkload,
        planned: &Planned,
        roots: &[QueryRoot],
        last_root_is_derived: bool,
    ) -> Result<Costed, String> {
        let planning = workload.planning_workload();
        let entries: Vec<_> = planning.query_workload.entries().collect();
        if entries.len() != roots.len() {
            return Err(format!(
                "{} roots for {} queries",
                roots.len(),
                entries.len()
            ));
        }
        let (flat, trees) = flatten(roots);
        // The nodes each root reads, by id in `flat`.
        let reached_by_root: Vec<BTreeSet<usize>> = flat
            .roots
            .iter()
            .map(|root| {
                let mut seen = BTreeSet::new();
                let mut stack: Vec<usize> = root.operator_refs().into_iter().copied().collect();
                while let Some(node) = stack.pop() {
                    if seen.insert(node) {
                        stack.extend(flat.nodes[node].operator.children().into_iter().copied());
                    }
                }
                seen
            })
            .collect();
        let (last, others) = reached_by_root.split_last().ok_or("no roots")?;
        let shared: BTreeSet<usize> = match last_root_is_derived {
            true => {
                let grid = evaluation_grid_ms(&entries[entries.len() - 1]);
                others
                    .iter()
                    .zip(&entries)
                    .filter(|(_, entry)| evaluation_grid_ms(entry) == grid)
                    .flat_map(|(reached, _)| reached.intersection(last).copied())
                    .collect()
            }
            false => BTreeSet::new(),
        };
        let mut nodes: Vec<PhysicalASAPDAGNode> = flat
            .nodes
            .into_iter()
            .zip(&trees)
            .enumerate()
            .map(|(id, (flat_node, tree))| {
                let mut node = match planned.node_of.get(&Rc::as_ptr(tree)) {
                    Some(&planned_id) => PhysicalASAPDAGNode {
                        id,
                        payload: flat_node.operator,
                        ..planned.plan.nodes[planned_id].clone()
                    },
                    None => PhysicalASAPDAGNode {
                        id,
                        output_state: ExecutionDataState {
                            timing: ExecutionTiming::QueryTime,
                            primitive: DataPrimitive::of(&flat_node.operator, tree.result_kind),
                        },
                        payload: flat_node.operator,
                        lifetime: Lifetime::None,
                        parameters: None,
                        output_schema: tree.schema.clone(),
                        guarantee: tree.guarantee.clone(),
                        retention_ms: None,
                        summary_coverage: None,
                        selected_physical_candidate: None,
                    },
                };
                if shared.contains(&id)
                    && node.output_state.timing == ExecutionTiming::QueryTime
                    && node.lifetime == Lifetime::None
                {
                    node.lifetime = Lifetime::OneEvaluation(StorageMedium::Memory);
                }
                node
            })
            .collect();
        for (id, tree) in trees.iter().enumerate() {
            let Some(NonASAPOp::TimeRange { range, .. }) = tree.non_asap() else {
                continue;
            };
            if planned.node_of.contains_key(&Rc::as_ptr(tree)) {
                continue;
            }
            let range_ms = u64::try_from(range.as_millis()).unwrap_or(u64::MAX);
            for &child in nodes[id].payload.clone().children() {
                if let Some(retention_ms) = &mut nodes[child].retention_ms {
                    *retention_ms = (*retention_ms).max(range_ms);
                }
            }
        }
        let mut operator_roots = Vec::new();
        let mut scalar_roots = Vec::new();
        for (query, root) in flat.roots.into_iter().enumerate() {
            match root {
                QueryRoot::Operator(id) => operator_roots.push(id),
                QueryRoot::Scalar(expression) => scalar_roots.push((query, expression)),
            }
        }
        let dag = PhysicalASAPDAG::new(nodes, operator_roots, scalar_roots)
            .map_err(|e| format!("witness: {e}"))?;
        let (cost, _) = plan_cost(&dag, &entries, &workload.data, &self.cost_inputs())
            .map_err(|(node, e)| format!("witness: node {node}: {e}"))?;
        let cost_per_second = cost
            .cost_per_second
            .ok_or("no cost per second (cost model 3)")?;
        let private: Vec<_> = last
            .iter()
            .filter(|node| others.iter().all(|reached| !reached.contains(node)))
            .filter_map(|node| cost.per_node.get(node))
            .collect();
        Ok(Costed {
            selected: plan_label(&dag),
            cost_per_second,
            derived_root: PrivateNodes {
                cost_per_second: private.iter().filter_map(|n| n.cost_per_second).sum(),
                operators: private
                    .iter()
                    .map(|n| format!("{:?}", n.operator))
                    .collect(),
            },
        })
    }

    /// [`cost`](Self::cost) of `planned`, `workload`'s selected plan,
    /// cached per workload: every marginal check of a case costs the same
    /// `W`.
    pub fn reference_cost(
        &mut self,
        workload: &FuzzWorkload,
        planned: &Planned,
    ) -> Result<Costed, String> {
        let key = serde_json::to_string(workload).expect("a workload serializes");
        if let Some(costed) = self.reference_costs.get(&key) {
            return costed.clone();
        }
        let costed = self.cost(workload, planned, &planned.roots, false);
        self.reference_costs.insert(key, costed.clone());
        costed
    }
}
