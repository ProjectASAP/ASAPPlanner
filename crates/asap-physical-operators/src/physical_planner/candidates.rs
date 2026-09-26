//! Compile maintenance-selected frontiers without deployment-specific graph rewrites.
use super::*;

/// One computation realization; lifecycle/window/revision requirements accompany
/// it during optimization and deployment. Stored outputs have no storage identity.
#[derive(Clone)]
pub struct PhysicalCandidate {
    pub precompute: Option<CompiledPhysicalDag>,
    pub query: CompiledPhysicalDag,
    pub materialized_outputs: BTreeMap<NodeId, InputContract>,
}

/// Compile an explicit materialization frontier selected by Planner maintenance
/// search. Operators upstream of that frontier run in precompute, including
/// readouts/reductions; query execution receives their typed output values.
/// Empty frontiers retain the full computation in the query DAG.
///
/// Repeated windows must be instantiated with the same evaluation/population
/// contract used to build each output. This API never treats a result from a
/// different window or revision as interchangeable merely because types match.
pub fn compile_candidate(
    dag: &ExecutableDag,
    inputs: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
    frontier: &[NodeId],
) -> Result<PhysicalCandidate, Error> {
    if frontier.is_empty() {
        return Ok(PhysicalCandidate {
            precompute: None,
            query: compile(dag, inputs, roots)?,
            materialized_outputs: BTreeMap::new(),
        });
    }
    let frontier_set: BTreeSet<_> = frontier.iter().copied().collect();
    if frontier_set.len() != frontier.len() || frontier.iter().any(|id| inputs.contains_key(id)) {
        return Err(invalid("frontier must contain distinct computed outputs"));
    }
    let full = compile(dag, inputs.clone(), roots)?;
    let precompute = compile(dag, inputs.clone(), frontier)?;
    let mut materialized_outputs = BTreeMap::new();
    for &id in frontier {
        // Also proves that the frontier is reachable from the requested roots.
        full.output_contract(id)?;
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
    let query = compile(dag, query_inputs, roots)?;
    let used: BTreeSet<_> = query.input_contracts().map(|(id, _)| id).collect();
    if !frontier.iter().all(|id| used.contains(id)) {
        return Err(invalid(
            "frontier contains an output shadowed by another boundary",
        ));
    }
    Ok(PhysicalCandidate {
        precompute: Some(precompute),
        query,
        materialized_outputs,
    })
}

/// Enumerate bounded, reachable materialization frontiers above explicit inputs.
/// Each frontier is an antichain: storing an output and its ancestor together
/// would leave the ancestor unused by query execution. Lifecycle eligibility
/// and deployment feasibility are evaluated separately before cost selection.
/// Exceeding the search budget returns an error, never a partial inventory.
pub fn enumerate_frontiers(
    dag: &ExecutableDag,
    inputs: &BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
    max_candidates: usize,
) -> Result<Vec<Vec<NodeId>>, Error> {
    if max_candidates == 0 {
        return Err(invalid(
            "frontier search requires a positive candidate budget",
        ));
    }
    let compiled = compile(dag, inputs.clone(), roots)?;
    let mut ancestors = BTreeMap::<NodeId, BTreeSet<NodeId>>::new();
    let mut eligible = Vec::new();
    for node in &dag.nodes {
        let id = u64::from(node.id.0);
        if inputs.contains_key(&id) {
            continue;
        }
        let Ok(contract) = compiled.output_contract(id) else {
            continue;
        };
        if contract.properties.boundedness != Boundedness::Bounded {
            continue;
        }
        let mut seen = BTreeSet::new();
        let mut pending = vec![id];
        while let Some(current) = pending.pop() {
            if !seen.insert(current) || inputs.contains_key(&current) {
                continue;
            }
            pending.extend(
                dag.edges
                    .iter()
                    .filter(|edge| u64::from(edge.consumer.0) == current)
                    .map(|edge| u64::from(edge.producer.0)),
            );
        }
        ancestors.insert(id, seen);
        eligible.push(id);
    }
    eligible.sort_unstable();
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
pub fn compile_candidates(
    dag: &ExecutableDag,
    inputs: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
    frontiers: &[Vec<NodeId>],
) -> Vec<Result<PhysicalCandidate, Error>> {
    frontiers
        .iter()
        .map(|frontier| compile_candidate(dag, inputs.clone(), roots, frontier))
        .collect()
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

pub struct CandidateSelection {
    pub candidate: PhysicalCandidate,
    pub candidate_index: usize,
    pub cost: CandidateCost,
}

/// Select only compiled and deployment-feasible physical candidates. `None`
/// rejects an unbindable candidate before pricing. Comparable scoped costs are
/// required; deployment never rewrites the selected frontier after this step.
pub fn select_candidate(
    candidates: Vec<Result<PhysicalCandidate, Error>>,
    mut evaluate: impl FnMut(&PhysicalCandidate) -> Result<Option<CandidateCost>, Error>,
) -> Result<CandidateSelection, Error> {
    let mut scope: Option<(String, f64)> = None;
    let mut selected: Option<CandidateSelection> = None;
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
