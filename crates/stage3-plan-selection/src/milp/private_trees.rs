//! Search-space pruning rule 7 (`SearchSpacePruning::single_reader_dp`):
//! a region of the plan only one reader can read, with no term that couples
//! it to the rest of the selection, is solved by dynamic programming, and
//! only its cheapest tree reaches the MILP.
//!
//! An execution is *private* when one selectable edge reads it and no query
//! output does, its candidate is exact (θ = None: no error for (K) to
//! compose), nothing it holds is charged outside its own cost (no
//! retention $h_v$, raw data on disk, kept results or resident windows),
//! and every producer on its inputs is private too. A private execution's
//! cheapest tree costs its own cost plus, per input, the cheapest edge and
//! tree below it. Where every producer on a consumer's input is private,
//! each producer's tree is read by that input alone, so a plan reading a
//! dearer one reads the cheapest in its place at no more cost and stays
//! feasible: the dearer producers and their trees are left out.
//!
//! Example: `sum(sum_over_time(m[5m]))` with raw data retained: the
//! query-time `TimeRange` over the raw scan and the exact `SummaryAgg`
//! alternatives below the sum are private; only the cheapest tree for each
//! consumer input stays, while the readout groups a sketch's accuracy
//! couples (θ), and the shared core, stay in the MILP.

use super::model::SelectionInputs;
use crate::cost::ExecutionCosts;

/// Per execution, by position, whether it is left out as a dearer private
/// tree (module doc), among the executions `kept` (the others are already
/// left out). Without accuracy targets (`accuracy_enforced` false, no
/// (K)), a sketch candidate's θ couples nothing, so it may be private too.
pub(crate) fn dearer_private_trees(
    inputs: &SelectionInputs<'_>,
    accuracy_enforced: bool,
    kept: &[bool],
) -> Vec<bool> {
    let selectable = inputs.selectable;
    let executions = selectable.executions();
    let count = executions.len();
    let costs = inputs.costs;
    let mut answers = vec![false; count];
    for (_, answering) in selectable.answering() {
        for &index in answering {
            answers[index] = true;
        }
    }
    let locally_uncoupled = |index: usize| {
        kept[index] && uncoupled(inputs, costs, index, answers[index], accuracy_enforced)
    };
    // The kept producer of each selectable edge, `None` for one left out.
    let kept_producer = |position: usize| {
        let producer = selectable.edges()[position].producer;
        kept[producer].then_some(producer)
    };
    // Private executions and their cheapest trees, producers first: an
    // execution is decided once every producer on its inputs is.
    let mut tree: Vec<Option<f64>> = vec![None; count];
    let mut decided = vec![false; count];
    let mut changed = true;
    while changed {
        changed = false;
        for index in 0..count {
            if decided[index] {
                continue;
            }
            let producers_decided = (0..selectable.input_count(index)).all(|input| {
                selectable
                    .edges_into(index, input)
                    .iter()
                    .filter_map(|&position| kept_producer(position))
                    .all(|producer| decided[producer])
            });
            if !producers_decided && locally_uncoupled(index) {
                continue;
            }
            decided[index] = true;
            changed = true;
            if !locally_uncoupled(index) {
                continue;
            }
            tree[index] = private_tree_cost(inputs, index, &tree, kept);
        }
    }
    // Where every producer on an input is private, all but the cheapest
    // (the first among equals) are left out, with their trees.
    let mut left_out = vec![false; count];
    for consumer in 0..count {
        for input in 0..selectable.input_count(consumer) {
            if !kept[consumer] {
                continue;
            }
            let reads: Vec<(usize, f64)> = selectable
                .edges_into(consumer, input)
                .iter()
                .filter_map(|&position| {
                    let producer = kept_producer(position)?;
                    Some((producer, costs.per_edge[position]? + tree[producer]?))
                })
                .collect();
            let every_producer_private = selectable
                .edges_into(consumer, input)
                .iter()
                .filter_map(|&position| kept_producer(position))
                .all(|producer| tree[producer].is_some());
            if !every_producer_private || reads.len() < 2 {
                continue;
            }
            let cheapest = reads
                .iter()
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .expect("two reads")
                .0;
            for &(producer, _) in &reads {
                if producer != cheapest {
                    leave_out_tree(inputs, producer, &tree, kept, &mut left_out);
                }
            }
        }
    }
    left_out
}

/// Whether execution `index` has no term coupling it to the rest of the
/// selection: one selectable edge reads it and no query output
/// (`answers`), its candidate is exact (or accuracy is not enforced), and
/// it has no retention, raw data on disk, kept results or resident
/// windows.
fn uncoupled(
    inputs: &SelectionInputs<'_>,
    costs: &ExecutionCosts,
    index: usize,
    answers: bool,
    accuracy_enforced: bool,
) -> bool {
    let selectable = inputs.selectable;
    let v = selectable.executions()[index].candidate;
    let logical = inputs.physical.physical_candidates[v.0].logical_candidate_id;
    !answers
        && selectable.edges_out_of(index).len() == 1
        && costs.per_execution[index].is_ok()
        && (!accuracy_enforced || {
            let candidate = inputs.physical.logical_candidates.memo.candidate(logical);
            candidate.logical_properties.parameters().is_none()
                && candidate
                    .input_requirements
                    .iter()
                    .all(|requirement| requirement.required_accuracy.is_none())
        })
        // (X): no query must avoid reading it.
        && (0..inputs.entries.len())
            .all(|query| !inputs.physical.logical_candidates.query_must_not_read(query, logical))
        && !costs.retention_cost_per_retained_second.contains_key(&v)
        && !costs.raw_data_source.contains_key(&v)
        && !costs.per_resident_window.contains_key(&v)
        && costs
            .kept_result_bytes_per_execution
            .get(index)
            .copied()
            .unwrap_or(0.0)
            == 0.0
        && costs
            .answer_bytes_per_execution
            .get(index)
            .copied()
            .unwrap_or(0.0)
            == 0.0
        && !costs
            .raw_data_reads
            .iter()
            .any(|read| read.execution == index)
        && !costs
            .resident_reads
            .iter()
            .any(|read| read.execution == index)
}

/// The cheapest tree of uncoupled execution `index`, every kept producer
/// on its inputs decided (`tree`): its own cost plus, per input, the
/// cheapest costed edge and tree; `None` when a kept producer is not
/// private or an input has no costed read.
fn private_tree_cost(
    inputs: &SelectionInputs<'_>,
    index: usize,
    tree: &[Option<f64>],
    kept: &[bool],
) -> Option<f64> {
    let selectable = inputs.selectable;
    let costs = inputs.costs;
    let own = *costs.per_execution[index].as_ref().ok()?;
    let mut total = own;
    for input in 0..selectable.input_count(index) {
        let mut cheapest = f64::INFINITY;
        for &position in selectable.edges_into(index, input) {
            let producer = selectable.edges()[position].producer;
            if kept[producer] {
                cheapest = cheapest.min(costs.per_edge[position]? + tree[producer]?);
            }
        }
        if !cheapest.is_finite() {
            return None;
        }
        total += cheapest;
    }
    Some(total)
}

/// Leaves out private execution `index` and its private subtree, the
/// kept private producers below it (`tree` is `Some` for a private
/// execution): each is read by its one edge only, so nothing else reads
/// them. A producer already left out, or not private, is not walked: it
/// may be shared.
fn leave_out_tree(
    inputs: &SelectionInputs<'_>,
    index: usize,
    tree: &[Option<f64>],
    kept: &[bool],
    left_out: &mut [bool],
) {
    let selectable = inputs.selectable;
    let mut pending = vec![index];
    while let Some(execution) = pending.pop() {
        if std::mem::replace(&mut left_out[execution], true) {
            continue;
        }
        for input in 0..selectable.input_count(execution) {
            pending.extend(
                selectable
                    .edges_into(execution, input)
                    .iter()
                    .map(|&position| selectable.edges()[position].producer)
                    .filter(|&producer| kept[producer] && tree[producer].is_some()),
            );
        }
    }
}
