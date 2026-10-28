//! `planner_fuzz`: a metamorphic fuzz tester for missed reuse and sharing.
//!
//! A missed-reuse bug is a plan the planner's rules never generate: the
//! answers stay correct, and an exhaustive search over the generated
//! candidates cannot see it. The tester finds one by relating two
//! workloads. For a query `q` of a workload `W` and a query `q'` derivable
//! from `q`'s result or state (the same query with a looser accuracy
//! target, a filter on a group key, a coarser grouping, ...), it builds a
//! *witness*: `W`'s selected logical plan plus `q'` derived from `q`'s
//! node by one operator. Stage 2 and Stage 3 implement and price the
//! witness exactly as they price any candidate
//! ([`optimize_physical_asap`], [`select_plan`]), and the relation
//!
//! ```text
//! cost(W ∪ {q'}) − cost(W) ≤ derive_cost + tolerance,
//! derive_cost = cost(witness) − cost(W's plan priced the same way)
//! ```
//!
//! must hold. `derive_cost` is what Stage 3 charges for the derived
//! operator alone (its per-node price, plus any change it causes in the
//! shared nodes' prices, such as a shared node now materialized); it is
//! never a guess. A violation means a cheaper plan for `W ∪ {q'}` exists
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_physical_asap_optimization::optimize_physical_asap;
use asap_plan_selection::pass::Stage1ToStage3Search;
use asap_plan_selection::{select_plan, PlanSelectionSettings, PlanningModels, SearchAlgorithm};
use asap_planner::PlannerExecutorCompileCheck;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::sketch::SketchAlgorithm;
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    DataArrival, DataWorkload, PlanningWorkload, QueryLanguage, QueryWorkload, RepeatingEntry,
    SqlDialect,
};

use crate::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_planning_stages_over, run_search_over, sql_catalog,
};
use generate::QuerySpec;

/// The language of a fuzz workload: one per workload, as one
/// `stage_pipeline --workload` file has.
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
            "demand": {"fixed_interval": repeat_interval_ms},
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
/// the SQL tables they read (`stage_pipeline --table` objects) and whether
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
    /// The `PlanningWorkload` (`stage_pipeline --workload` file) of these
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
    /// (`series_count` per `sample_interval`), when every source declares
    /// one.
    pub fn set_arrival_from_sources(&mut self) {
        let mut rows_per_second = 0.0;
        for facts in self.data.sources.values() {
            match (facts.series_count, facts.sample_interval) {
                (Some(series), Some(interval)) if interval.0 > 0 => {
                    rows_per_second += series as f64 * 1_000.0 / interval.0 as f64
                }
                _ => return,
            }
        }
        if rows_per_second > 0.0 {
            self.data.arrival = DataArrival::Ingesting { rows_per_second };
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
                            .is_none_or(|only| only.contains(&supported.family))
                    })
                    .collect(),
            ),
            raw_data_retained: self.raw_data_retained,
            query_time_results_retained: self.query_time_results_retained,
        }
    }

    /// The `stage_pipeline` invocation that plans this workload, saved as
    /// `workload_file`, with `settings`.
    /// Its output document is `<workload_file stem>.out.json`.
    pub fn stage_pipeline_command(&self, workload_file: &str, settings: &FuzzSettings) -> String {
        let mut command = format!(
            "cargo run -p asap-devtools --bin stage_pipeline -- --workload {workload_file}"
        );
        for table in &self.tables {
            command.push_str(&format!(" \\\n  --table '{table}'"));
        }
        if settings.enforce_accuracy_guarantees {
            command.push_str(" --enforce-accuracy-guarantees");
        }
        if !self.raw_data_retained || self.query_time_results_retained {
            command.push_str(&format!(
                " --deployment-capabilities '{{\"raw_data_retained\": {}, \"query_time_results_retained\": {}}}'",
                self.raw_data_retained, self.query_time_results_retained
            ));
        }
        match settings.search {
            SearchKind::CandidateConnections => {}
            SearchKind::Exhaustive => command.push_str(" --exhaustive"),
        }
        let stem = workload_file.strip_suffix(".json").unwrap_or(workload_file);
        command.push_str(&format!(" --out {stem}.out.json"));
        command
    }
}

/// Workloads read from a seed file: a `PlanningWorkload` JSON (its
/// repeating queries), with the SQL tables of a `tables.json` (a JSON array
/// of `--table` objects) in the same directory, or an object
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

/// Which search plans each workload (`stage_pipeline`'s choices).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchKind {
    /// Joint candidate-connection planning (`stage_pipeline`'s default).
    CandidateConnections,
    /// Every generated candidate (`--exhaustive`), up to
    /// [`EXHAUSTIVE_MAX_COMBINATIONS`].
    Exhaustive,
}

/// `--exhaustive`'s default evaluated-assignment budget.
pub const EXHAUSTIVE_MAX_COMBINATIONS: usize = 10_000;

/// How the tester plans and judges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuzzSettings {
    pub search: SearchKind,
    /// Whether Stage 3 rejects an output whose accuracy guarantee misses its
    /// target, as `plan_installable` does (default `true`).
    pub enforce_accuracy_guarantees: bool,
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
            enforce_accuracy_guarantees: true,
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
    /// The selected physical plan's stable archive ID.
    pub selected: String,
    /// Its price per second under cost model 1.
    pub cost_per_second: f64,
    /// The frontends' roots, one per query.
    pub stage0_roots: Vec<QueryRoot>,
    /// The selected Stage 1 (logical) candidate's roots, the plan a witness
    /// extends; absent only when the selected logical plan is unavailable.
    pub logical_roots: Option<Vec<QueryRoot>>,
}

/// A plan priced by Stage 2 and Stage 3 outside the search: the cheapest
/// materialization assignment of given logical roots.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Priced {
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

/// Plans and prices workloads, caching each workload's plan and the
/// reference price of its selected logical plan.
pub struct Planner {
    pub settings: FuzzSettings,
    cache: HashMap<String, Result<Rc<Planned>, String>>,
    reference_prices: HashMap<String, Result<Priced, String>>,
}

impl Planner {
    pub fn new(settings: FuzzSettings) -> Self {
        Self {
            settings,
            cache: HashMap::new(),
            reference_prices: HashMap::new(),
        }
    }

    fn plan_selection_settings(&self) -> PlanSelectionSettings {
        PlanSelectionSettings {
            ignore_accuracy_guarantees: !self.settings.enforce_accuracy_guarantees,
            ..PlanSelectionSettings::default()
        }
    }

    /// The frontends' roots of `workload`, one per query.
    pub fn lower(&self, workload: &FuzzWorkload) -> Result<Vec<QueryRoot>, String> {
        let planning = workload.planning_workload();
        match workload.language {
            Language::Promql => lower_promql_roots(&planning),
            Language::Sql => {
                let tables: Vec<String> = workload.tables.iter().map(Value::to_string).collect();
                lower_sql_roots(&planning, &sql_catalog(&tables)?)
            }
        }
    }

    /// `workload` planned by the configured search, as `stage_pipeline`
    /// plans it.
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
        let plan_selection = self.plan_selection_settings();
        let deployment = workload.deployment();
        let models = PlanningModels::builtin()
            .with_plan_selection(&plan_selection)
            .with_deployment_capabilities(&deployment)
            .with_executor_compile_check(&PlannerExecutorCompileCheck);
        let run = match self.settings.search {
            SearchKind::Exhaustive => run_planning_stages_over(
                roots.clone(),
                &entries,
                &data,
                Some(EXHAUSTIVE_MAX_COMBINATIONS),
                models,
            ),
            SearchKind::CandidateConnections => run_search_over(
                roots.clone(),
                &entries,
                &data,
                models,
                Stage1ToStage3Search {
                    search: SearchAlgorithm::CandidateConnections,
                    ..Stage1ToStage3Search::default()
                },
            ),
        }
        .map_err(|e| e.to_string())?;
        let selection = run
            .stage3_plan_selection
            .as_ref()
            .map_err(|e| format!("selection: {e}"))?;
        let cost_per_second = selection.costs[&selection.selected]
            .cost_per_second
            .ok_or("no cost per second (cost model 3)")?;
        let from_logical = run
            .stage2_physical_asap_optimization
            .iter()
            .find(|p| p.id == selection.selected)
            .map(|p| p.from_logical.clone());
        let logical_roots = from_logical.and_then(|id| {
            run.stage1_logical_asap_optimization
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.roots.as_ref().ok().cloned())
        });
        Ok(Planned {
            selected: selection.selected.clone(),
            cost_per_second,
            stage0_roots: roots,
            logical_roots,
        })
    }

    /// Stage 2 and Stage 3 over the logical plan `roots` of `workload`'s
    /// queries (one root per query, in order): every materialization
    /// assignment implemented and priced, the cheapest selected, with the
    /// same models as [`plan`](Self::plan). Guarantees are composed again
    /// first, so a derived node carries the guarantee of what it reads.
    /// With `last_root_is_key_filter`, the last root is a `Filter` on
    /// group-key columns only and keeps its input's guarantee (whole groups
    /// selected by an exact key keep each remaining row's bound), which
    /// Stage 1's composition, giving no guarantee to any filter over
    /// approximate rows, does not know.
    pub fn price(
        &self,
        workload: &FuzzWorkload,
        roots: &[QueryRoot],
        last_root_is_key_filter: bool,
    ) -> Result<Priced, String> {
        for root in roots {
            root.validate_structure()
                .map_err(|e| format!("witness validation: {e}"))?;
        }
        let mut roots =
            asap_logical_asap_optimization::per_computation_candidates::recompose_guarantees(roots)
                .map_err(|e| format!("witness guarantees: {e}"))?;
        if let (true, Some(last)) = (last_root_is_key_filter, roots.last_mut()) {
            let filter = last.as_operator().ok_or("a scalar root")?;
            let input = filter.operator.children()[0].guarantee.clone();
            *last = QueryRoot::Operator(Rc::new((**filter).clone().with_guarantee(input)));
        }
        let planning = workload.planning_workload();
        let entries: Vec<_> = planning.query_workload.entries().collect();
        let plan_selection = self.plan_selection_settings();
        let deployment = workload.deployment();
        let models = PlanningModels::builtin()
            .with_plan_selection(&plan_selection)
            .with_deployment_capabilities(&deployment)
            .with_executor_compile_check(&PlannerExecutorCompileCheck);
        let mut candidates =
            optimize_physical_asap("witness", &roots, &entries, &workload.data, &deployment)
                .map_err(|e| format!("witness Stage 2: {e}"))?;
        for (index, candidate) in candidates.iter_mut().enumerate() {
            candidate.id = format!("witness-m{index}");
        }
        let selection = select_plan(&candidates, &entries, &workload.data, models)
            .map_err(|e| format!("witness Stage 3: {e}"))?;
        let cost = &selection.costs[&selection.selected];
        let cost_per_second = cost
            .cost_per_second
            .ok_or("no cost per second (cost model 3)")?;
        let candidate = candidates
            .iter()
            .find(|c| c.id == selection.selected)
            .expect("the selected candidate is offered");
        let dag = &candidate.dag;
        let mut inputs: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for edge in &dag.edges {
            inputs.entry(edge.consumer).or_default().push(edge.producer);
        }
        let reached = |root| {
            let mut seen = BTreeSet::new();
            let mut stack = vec![root];
            while let Some(node) = stack.pop() {
                if seen.insert(node) {
                    stack.extend(inputs.get(&node).into_iter().flatten().copied());
                }
            }
            seen
        };
        let reached_by_root: Vec<BTreeSet<_>> = dag.roots.iter().map(|r| reached(*r)).collect();
        let (last, others) = reached_by_root.split_last().ok_or("no roots")?;
        let private: Vec<_> = last
            .iter()
            .filter(|node| others.iter().all(|set| !set.contains(node)))
            .filter_map(|node| cost.per_node.get(node))
            .collect();
        let derived_root = PrivateNodes {
            cost_per_second: private.iter().filter_map(|n| n.cost_per_second).sum(),
            operators: private
                .iter()
                .map(|n| format!("{:?}", n.operator))
                .collect(),
        };
        Ok(Priced {
            selected: selection.selected.clone(),
            cost_per_second,
            derived_root,
        })
    }

    /// [`price`](Self::price) of `workload`'s selected logical plan
    /// `roots`, cached per workload: every marginal check of a case prices
    /// the same `W`.
    pub fn reference_price(
        &mut self,
        workload: &FuzzWorkload,
        roots: &[QueryRoot],
    ) -> Result<Priced, String> {
        let key = serde_json::to_string(workload).expect("a workload serializes");
        if let Some(priced) = self.reference_prices.get(&key) {
            return priced.clone();
        }
        let priced = self.price(workload, roots, false);
        self.reference_prices.insert(key, priced.clone());
        priced
    }
}
