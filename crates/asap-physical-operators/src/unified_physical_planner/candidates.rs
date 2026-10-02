//! Compile maintenance-selected frontiers without deployment-specific dag rewrites.
use super::*;

/// One computation realization; lifecycle/window/revision requirements accompany
/// it during optimization and deployment. Stored outputs have no storage identity.
/// Deserialization validates the producer/reader boundary.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "UncheckedPhysicalASAPDAG")]
pub struct PhysicalASAPDAG {
    pub precompute: Option<CompiledPhysicalDAG>,
    pub query: CompiledPhysicalDAG,
    pub materialized_outputs: BTreeMap<NodeId, InputContract>,
}

/// Compile an explicit materialization frontier selected by Planner maintenance
/// search. Operators upstream of that frontier run in precompute, including
/// evaluations/reductions; query execution receives their typed output values.
/// Empty frontiers retain the full computation in the query DAG.
///
/// Repeated windows must be instantiated with the same evaluation/population
/// contract used to build each output. This API never treats a result from a
/// different window or revision as interchangeable merely because types match.
pub fn compile_candidate(
    dag: &PostAsapDAG,
    inputs: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
    frontier: &[NodeId],
) -> Result<PhysicalASAPDAG, Error> {
    cut_candidate(&compile(dag, inputs, roots)?, frontier)
}

/// Derive one frontier's candidate from a complete [`compile`] result by
/// partitioning its operators; nothing is lowered again. A deployment compiles
/// each query DAG once and derives every placement choice from that result.
/// The candidate is identical to [`compile_candidate`] for the same frontier.
pub fn cut_candidate(
    compiled: &CompiledPhysicalDAG,
    frontier: &[NodeId],
) -> Result<PhysicalASAPDAG, Error> {
    if frontier.is_empty() {
        return Ok(PhysicalASAPDAG {
            precompute: None,
            query: compiled.clone(),
            materialized_outputs: BTreeMap::new(),
        });
    }
    let frontier_set: BTreeSet<_> = frontier.iter().copied().collect();
    // `compile` retains only reachable nodes and numbers its helper operators
    // above the u32 Planner ID range; only Planner outputs are boundaries.
    if frontier_set.len() != frontier.len()
        || frontier
            .iter()
            .any(|&id| !compiled.is_operator(id) || u32::try_from(id).is_err())
    {
        return Err(invalid("frontier must contain distinct computed outputs"));
    }
    let inputs: BTreeMap<_, _> = compiled
        .input_contracts()
        .map(|(id, contract)| (id, contract.clone()))
        .collect();
    let precompute = compiled.cut(&inputs, frontier)?;
    let mut materialized_outputs = BTreeMap::new();
    for &id in frontier {
        let mut output = precompute.output_contract(id)?;
        if output.properties.boundedness != Boundedness::Bounded {
            return Err(invalid("materialized output requires bounded execution"));
        }
        // A stored reader may stream batches even when the producer blocked.
        // Its timing is independent; the retained result still must be finite.
        output.properties.emission = Emission::Unknown;
        materialized_outputs.insert(id, output);
    }
    let mut query_inputs = inputs;
    query_inputs.extend(materialized_outputs.clone());
    let query = compiled.cut(&query_inputs, compiled.roots())?;
    let used: BTreeSet<_> = query.input_contracts().map(|(id, _)| id).collect();
    if !frontier.iter().all(|id| used.contains(id)) {
        return Err(invalid(
            "frontier contains an output shadowed by another boundary",
        ));
    }
    Ok(PhysicalASAPDAG {
        precompute: Some(precompute),
        query,
        materialized_outputs,
    })
}

/// Materialization frontier implied by lifecycle-assigned timing: ingestion-time
/// nodes read by a query-time node, plus the root when it is ingestion-timed.
/// `cut_candidate` of one [`compile`] result with this frontier realizes the
/// assignment, so different assignments are different cuts of one lowering.
/// That holds while timing-dependent lowering (an ingestion-time `Binary`
/// aligns by value column) has the same timing at compile time as here.
/// A query-time node feeding an ingestion-time node has no valid placement.
pub fn frontier_from_timing(dag: &PostAsapDAG) -> Result<Vec<NodeId>, Error> {
    use planner_types::post_asap::ExecutionTiming::IngestionTime;
    let timing = dag
        .nodes
        .iter()
        .map(|node| (node.id, node.output_state.timing))
        .collect::<BTreeMap<_, _>>();
    let mut frontier = BTreeSet::new();
    if timing.get(&dag.root) == Some(&IngestionTime) {
        frontier.insert(u64::from(dag.root.0));
    }
    for edge in &dag.edges {
        let (Some(&producer), Some(&consumer)) =
            (timing.get(&edge.producer), timing.get(&edge.consumer))
        else {
            return Err(invalid("timed DAG edge names an unknown node"));
        };
        match (producer == IngestionTime, consumer == IngestionTime) {
            (true, false) => {
                frontier.insert(u64::from(edge.producer.0));
            }
            (false, true) => return Err(invalid("query-time node feeds an ingestion-time node")),
            _ => {}
        }
    }
    Ok(frontier.into_iter().collect())
}

/// Enumerate bounded, reachable materialization frontiers above explicit inputs.
/// Each frontier is an antichain: storing an output and its ancestor together
/// would leave the ancestor unused by query execution. Lifecycle eligibility
/// and deployment feasibility are evaluated separately before cost selection.
/// Exceeding the search budget returns an error, never a partial inventory.
pub fn enumerate_frontiers(
    dag: &PostAsapDAG,
    inputs: &BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
    max_candidates: usize,
) -> Result<Vec<Vec<NodeId>>, Error> {
    enumerate_compiled_frontiers(&compile(dag, inputs.clone(), roots)?, max_candidates)
}

fn enumerate_compiled_frontiers(
    compiled: &CompiledPhysicalDAG,
    max_candidates: usize,
) -> Result<Vec<Vec<NodeId>>, Error> {
    if max_candidates == 0 {
        return Err(invalid(
            "frontier search requires a positive candidate budget",
        ));
    }
    let mut ancestors = BTreeMap::<NodeId, BTreeSet<NodeId>>::new();
    let mut eligible = Vec::new();
    for (id, properties) in compiled.output_properties()? {
        if !compiled.is_operator(id)
            || u32::try_from(id).is_err()
            || properties.boundedness != Boundedness::Bounded
        {
            continue;
        }
        let mut seen = BTreeSet::new();
        let mut pending = vec![id];
        while let Some(current) = pending.pop() {
            if seen.insert(current) {
                pending.extend(compiled.dependencies(current));
            }
        }
        ancestors.insert(id, seen);
        eligible.push(id);
    }
    let mut frontiers = vec![vec![]];
    for id in eligible {
        let additions = frontiers
            .iter()
            .filter(|frontier| {
                frontier.iter().all(|previous| {
                    !ancestors[&id].contains(previous) && !ancestors[previous].contains(&id)
                })
            })
            .map(|frontier| {
                let mut next = frontier.clone();
                next.push(id);
                next
            })
            .collect::<Vec<_>>();
        if additions.len() > max_candidates.saturating_sub(frontiers.len()) {
            return Err(invalid(
                "materialization frontier search exceeds candidate budget",
            ));
        }
        frontiers.extend(additions);
    }
    Ok(frontiers)
}

/// Lower every maintenance candidate before feasibility/cost evaluation. Keep
/// individual failures visible; do not substitute another computation on error.
/// The DAG is lowered once; each frontier is a [`cut_candidate`] of it.
pub fn compile_candidates(
    dag: &PostAsapDAG,
    inputs: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
    frontiers: &[Vec<NodeId>],
) -> Vec<Result<PhysicalASAPDAG, Error>> {
    match compile(dag, inputs, roots) {
        Ok(compiled) => frontiers
            .iter()
            .map(|frontier| cut_candidate(&compiled, frontier))
            .collect(),
        Err(error) => frontiers.iter().map(|_| Err(error.clone())).collect(),
    }
}

/// Complete workload cost supplied by scoped optimizer/deployment evidence.
/// The evaluator includes build/update work, retained state, shared producers
/// and recurrent reads over the same horizon; these are not per-query timings.
#[derive(Clone, Debug)]
pub struct CandidateCost {
    pub workload_scope: String,
    pub horizon_seconds: f64,
    pub total_cost: f64,
}

pub struct CandidateSelection<T = PhysicalASAPDAG> {
    pub candidate: T,
    pub candidate_index: usize,
    pub cost: CandidateCost,
}

/// Select only compiled and deployment-feasible physical candidates. `None`
/// rejects an unbindable candidate before pricing. Comparable scoped costs are
/// required; deployment never rewrites the selected frontier after this step.
/// The payload is generic so deployments can retain binding/diagnostic metadata
/// alongside each compiled computation without duplicating winner selection.
pub fn select_candidate<T>(
    candidates: Vec<Result<T, Error>>,
    mut evaluate: impl FnMut(&T) -> Result<Option<CandidateCost>, Error>,
) -> Result<CandidateSelection<T>, Error> {
    let mut scope: Option<(String, f64)> = None;
    let mut selected: Option<CandidateSelection<T>> = None;
    for (candidate_index, candidate) in candidates.into_iter().enumerate() {
        let Ok(candidate) = candidate else { continue };
        let Some(cost) = evaluate(&candidate)? else {
            continue;
        };
        if cost.workload_scope.is_empty()
            || !cost.horizon_seconds.is_finite()
            || cost.horizon_seconds <= 0.
            || !cost.total_cost.is_finite()
            || cost.total_cost < 0.
        {
            return Err(invalid(
                "candidate cost lacks a valid workload scope/horizon",
            ));
        }
        let current_scope = (cost.workload_scope.clone(), cost.horizon_seconds);
        if scope.as_ref().is_some_and(|scope| scope != &current_scope) {
            return Err(invalid(
                "candidate costs describe different workloads or horizons",
            ));
        }
        scope = Some(current_scope);
        if selected
            .as_ref()
            .is_none_or(|selected| cost.total_cost < selected.cost.total_cost)
        {
            selected = Some(CandidateSelection {
                candidate,
                candidate_index,
                cost,
            });
        }
    }
    selected.ok_or_else(|| invalid("no feasible priced physical candidate"))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedPhysicalASAPDAG {
    precompute: Option<CompiledPhysicalDAG>,
    query: CompiledPhysicalDAG,
    materialized_outputs: BTreeMap<NodeId, InputContract>,
}
impl TryFrom<UncheckedPhysicalASAPDAG> for PhysicalASAPDAG {
    type Error = Error;
    fn try_from(candidate: UncheckedPhysicalASAPDAG) -> Result<Self, Error> {
        let result = Self {
            precompute: candidate.precompute,
            query: candidate.query,
            materialized_outputs: candidate.materialized_outputs,
        };
        result.validate()?;
        Ok(result)
    }
}

impl PhysicalASAPDAG {
    /// Validate the physical handoff, including the producer/reader boundary.
    pub fn validate(&self) -> Result<(), Error> {
        self.query.validate()?;
        let Some(precompute) = &self.precompute else {
            return if self.materialized_outputs.is_empty() {
                Ok(())
            } else {
                Err(invalid("materialized outputs have no producer DAG"))
            };
        };
        precompute.validate()?;
        let outputs: BTreeSet<_> = self.materialized_outputs.keys().copied().collect();
        if outputs.is_empty() || outputs != precompute.roots().iter().copied().collect() {
            return Err(invalid("physical frontier differs from precompute outputs"));
        }
        let readers: BTreeMap<_, _> = self.query.input_contracts().collect();
        for (&id, contract) in &self.materialized_outputs {
            let produced = precompute.output_contract(id)?;
            // Direct frontiers retain their node IDs. Temporal candidates can
            // read several window instances through distinct input slots;
            // their deployment bindings must validate those slots separately.
            let reader = readers.get(&id);
            if contract.schema != produced.schema
                || reader.is_some_and(|reader| contract.schema != reader.schema)
                || produced.properties.boundedness != Boundedness::Bounded
                || contract.properties.boundedness != Boundedness::Bounded
                || reader
                    .is_some_and(|reader| reader.properties.boundedness != Boundedness::Bounded)
            {
                return Err(invalid("physical frontier schema or boundedness mismatch"));
            }
        }
        Ok(())
    }
}
