//! #1095 Step 6 (§2.0, "Decode and check"): the group selection MILP's raw
//! solution as a [`DagOfSelectedCandidates`], one node per candidate run.
//!
//! - A candidate computed in a context (`p[ctx, c] = 1`, a `NotMaterialized`
//!   query-time candidate in a query's or a shareable candidate's context,
//!   or a `NotMaterialized` ingestion-time candidate in `I`) is one node per
//!   context: selected in two contexts, it is two nodes; read by two
//!   readers in one context, it is one.
//! - A selected shareable candidate (`x[m] = 1`) is one node, whoever reads
//!   it.
//! - In each context, input `n` of a computed node (or of `m`'s own node in
//!   `m`'s context) reads the context's selected candidate among its
//!   producers: the node computed there, the shareable candidate's node, or
//!   the candidate's node in `I`.
//! - Output reference `r` is the selected output candidate in `r`'s query
//!   context.
//!
//! Only the nodes the outputs reach are kept. The MILP never selects an
//! unread candidate (row 4), so this drops nothing at a solution whose
//! objective equals `plan_cost`.

use std::collections::{BTreeMap, BTreeSet};

use asap_types::stages::{
    output_references, DagOfSelectedCandidates, PhysicalASAPMemoCandidates, PhysicalCandidateId,
    SelectedEdge, SelectedNodeId,
};

use crate::groups::{MilpInputs, ResultSlot, RunKind};

/// One run of a candidate in the solution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Run {
    /// Candidate `.1` computed in context `.0`.
    Computed(usize, PhysicalCandidateId),
    /// Shareable candidate `.0`.
    Shared(PhysicalCandidateId),
}

/// The selected candidates of a solution: per context, `p[ctx, c] = 1`, and
/// the shareable candidates with `x[m] = 1`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ChosenCandidates {
    pub(crate) chosen: Vec<BTreeSet<PhysicalCandidateId>>,
    pub(crate) selected: BTreeSet<PhysicalCandidateId>,
    /// Per multi-slot input `(context, reader, input)`, the slot it reads.
    pub(crate) slot_reads: BTreeMap<(usize, PhysicalCandidateId, usize), ResultSlot>,
}

/// The selection as a DAG of candidate runs over `physical` (module
/// documentation), or why it is not one (an output or an input with no
/// selected producer). The caller validates it
/// ([`DagOfSelectedCandidates::validate`]).
pub(crate) fn decode(
    physical: &PhysicalASAPMemoCandidates,
    inputs: &MilpInputs,
    chosen: &ChosenCandidates,
) -> Result<DagOfSelectedCandidates, String> {
    let ingestion = physical.query_roots.len();
    // The node a read of `producer` in `context` resolves to.
    let run_of = |context: usize, producer: PhysicalCandidateId| -> Option<Run> {
        if !chosen.chosen[context].contains(&producer) {
            return None;
        }
        let kind = inputs.kind[producer.0];
        if inputs.contexts[context].computes(kind) {
            return Some(Run::Computed(context, producer));
        }
        match kind {
            RunKind::Shareable => chosen
                .selected
                .contains(&producer)
                .then_some(Run::Shared(producer)),
            RunKind::Ingestion => chosen.chosen[ingestion]
                .contains(&producer)
                .then_some(Run::Computed(ingestion, producer)),
            RunKind::PerContext => None,
        }
    };
    // The run's context for its inputs, and its candidate.
    let inputs_of = |run: Run| -> Result<Vec<(usize, Run)>, String> {
        let (context, candidate) = match run {
            Run::Computed(context, candidate) => (context, candidate),
            Run::Shared(candidate) => (
                *inputs
                    .context_of_shareable
                    .get(&candidate)
                    .ok_or_else(|| format!("shareable candidate {} has no context", candidate.0))?,
                candidate,
            ),
        };
        let mut reads = Vec::new();
        for (input, producers) in inputs.producer_candidates_per_input[candidate.0]
            .iter()
            .enumerate()
        {
            let slot_read = match run {
                Run::Computed(..) => chosen.slot_reads.get(&(context, candidate, input)),
                Run::Shared(_) => None,
            };
            let producer = producers
                .iter()
                .filter(|producer| {
                    matches!(run, Run::Shared(_))
                        || inputs.slot[producer.0] != inputs.slot[candidate.0]
                })
                .filter(|producer| slot_read.is_none_or(|&slot| inputs.slot[producer.0] == slot))
                .find_map(|&producer| run_of(context, producer))
                .ok_or_else(|| {
                    format!(
                        "input {input} of candidate {} in context {context} has no selected producer",
                        candidate.0
                    )
                })?;
            reads.push((input, producer));
        }
        Ok(reads)
    };
    let outputs: Vec<Run> = output_references(&physical.query_roots)
        .into_iter()
        .enumerate()
        .map(|(reference, (query, _))| {
            inputs.output_candidates[reference]
                .iter()
                .find_map(|&candidate| run_of(query, candidate))
                .ok_or_else(|| format!("output reference {reference} has no selected candidate"))
        })
        .collect::<Result<_, _>>()?;
    // Number the runs the outputs reach, depth first from the outputs.
    let mut node_of: BTreeMap<Run, SelectedNodeId> = BTreeMap::new();
    let mut reads_of: BTreeMap<Run, Vec<(usize, Run)>> = BTreeMap::new();
    let mut candidate_of_node = Vec::new();
    let mut stack: Vec<Run> = outputs.iter().rev().copied().collect();
    while let Some(run) = stack.pop() {
        if node_of.contains_key(&run) {
            continue;
        }
        node_of.insert(run, SelectedNodeId(candidate_of_node.len()));
        candidate_of_node.push(match run {
            Run::Computed(_, candidate) | Run::Shared(candidate) => candidate,
        });
        let reads = inputs_of(run)?;
        stack.extend(reads.iter().map(|&(_, producer)| producer));
        reads_of.insert(run, reads);
    }
    let mut edges = Vec::new();
    for (run, &consumer) in &node_of {
        for &(input, producer) in &reads_of[run] {
            edges.push(SelectedEdge {
                producer: node_of[&producer],
                consumer,
                input,
            });
        }
    }
    edges.sort_by_key(|edge| (edge.consumer.0, edge.input));
    Ok(DagOfSelectedCandidates {
        candidate_of_node,
        edges,
        node_of_output_reference: outputs.iter().map(|run| node_of[run]).collect(),
    })
}
