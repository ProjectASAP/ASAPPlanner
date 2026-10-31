//! #1095 Step 6 (§2.0, "Decode and check"): the selection MILP's raw
//! solution as a [`DagOfSelectedCandidates`], one node per candidate run.
//!
//! - Owner `o`'s use of stateless candidate `k` (`s[o, k]`) is one node
//!   running `k`: two owners using `k` are two nodes.
//! - Active instance `(s, j)` (`x[s, j]`) is one node running `s`, whoever
//!   reads it.
//! - Each selected link is one edge into input `n` of its reader's node:
//!   `e[o, k, n, k']` from `o`'s node of `k'`, `b[o, k, n, (s', j')]` from
//!   the node of instance `(s', j')`. In an instance owner, the links whose
//!   reader is the instance's own candidate are the instance node's inputs.
//! - Output reference `r` is `o[r, k]`'s node in `r`'s query owner, or
//!   `ob[r, i]`'s instance node.
//!
//! Only the nodes the outputs reach are kept. The MILP never selects an
//! unread node (rows 3 and 4), so this drops nothing at a solution whose
//! objective equals `plan_cost`; the caller's cost equality check catches a
//! solution that pays for a node no output reaches.

use std::collections::{BTreeMap, HashMap};

use asap_types::stages::{
    output_references, DagOfSelectedCandidates, PhysicalASAPMemoCandidates, PhysicalCandidateId,
    SelectedEdge, SelectedNodeId,
};

use super::model::{ChosenOwner, ChosenProducer, SelectionMilpSolution};

/// One run of a candidate in the solution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Run {
    /// Owner `.0`'s use of stateless candidate `.1` (owners `0..Q` are the
    /// queries, `Q + n` the `n`th active instance).
    Use(usize, PhysicalCandidateId),
    /// Instance `.1` of shareable candidate `.0`.
    Instance(PhysicalCandidateId, usize),
}

/// `solution` as a DAG of candidate runs over `physical` (module
/// documentation). The caller validates it
/// ([`DagOfSelectedCandidates::validate`]).
pub(crate) fn decode(
    physical: &PhysicalASAPMemoCandidates,
    solution: &SelectionMilpSolution,
) -> DagOfSelectedCandidates {
    let query_count = solution.query_owners.len();
    // Per run, its inputs: `(input, producer run)`.
    let mut inputs_of_run: HashMap<Run, Vec<(usize, Run)>> = HashMap::new();
    let mut add_owner = |owner: usize, choices: &ChosenOwner, instance: Option<Run>| {
        for &(reader, input, producer) in &choices.links {
            let consumer = match instance {
                Some(Run::Instance(class, index)) if class == reader => Run::Instance(class, index),
                _ => Run::Use(owner, reader),
            };
            let producer = match producer {
                ChosenProducer::Stateless(candidate) => Run::Use(owner, candidate),
                ChosenProducer::Instance { class, index } => Run::Instance(class, index),
            };
            inputs_of_run
                .entry(consumer)
                .or_default()
                .push((input, producer));
        }
    };
    for (query, choices) in solution.query_owners.iter().enumerate() {
        add_owner(query, choices, None);
    }
    for (number, (class, index, choices)) in solution.instances.iter().enumerate() {
        add_owner(
            query_count + number,
            choices,
            Some(Run::Instance(*class, *index)),
        );
    }
    let outputs: Vec<Run> = output_references(&physical.query_roots)
        .into_iter()
        .zip(&solution.outputs)
        .map(|((query, _), output)| match *output {
            ChosenProducer::Stateless(candidate) => Run::Use(query, candidate),
            ChosenProducer::Instance { class, index } => Run::Instance(class, index),
        })
        .collect();
    // Number the runs the outputs reach, depth first from the outputs.
    let mut node_of: BTreeMap<Run, SelectedNodeId> = BTreeMap::new();
    let mut candidate_of_node = Vec::new();
    let mut stack: Vec<Run> = outputs.iter().rev().copied().collect();
    while let Some(run) = stack.pop() {
        if node_of.contains_key(&run) {
            continue;
        }
        node_of.insert(run, SelectedNodeId(candidate_of_node.len()));
        candidate_of_node.push(match run {
            Run::Use(_, candidate) | Run::Instance(candidate, _) => candidate,
        });
        stack.extend(
            inputs_of_run
                .get(&run)
                .into_iter()
                .flatten()
                .map(|&(_, producer)| producer),
        );
    }
    let mut edges = Vec::new();
    for (run, &consumer) in &node_of {
        for &(input, producer) in inputs_of_run.get(run).into_iter().flatten() {
            edges.push(SelectedEdge {
                producer: node_of[&producer],
                consumer,
                input,
            });
        }
    }
    edges.sort_by_key(|edge| (edge.consumer.0, edge.input));
    DagOfSelectedCandidates {
        candidate_of_node,
        edges,
        node_of_output_reference: outputs.iter().map(|run| node_of[run]).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::milp::model::tests::{
        approximate, cost_inputs, shared_sum, through_stages, two_windows, Fixture,
    };
    use crate::milp::model::{
        build_selection_milp, solve_selection_milp, SelectionMilpOptions, SelectionMilpSize,
        SelectionMilpStatus,
    };
    use crate::owners::prepare_milp_inputs;

    fn solve(fixture: &Fixture) -> SelectionMilpSolution {
        let costs = cost_inputs();
        let inputs =
            prepare_milp_inputs(&fixture.physical, &fixture.entries, &fixture.data, &costs);
        let milp = build_selection_milp(
            &fixture.physical,
            &inputs,
            &fixture.entries,
            &costs,
            SelectionMilpOptions {
                enforce_accuracy: true,
            },
        )
        .unwrap();
        solve_selection_milp(milp, None, 0).unwrap()
    }

    // Round trip on C4's fixtures: the decoded DAG is valid, has one node
    // per owner's stateless use and per active instance and one edge per
    // selected link, and each node's candidate and inputs are the
    // solution's.
    #[test]
    fn decode_round_trips_the_solutions_of_the_c4_fixtures() {
        let ratio = through_stages(
            &[approximate(
                "quantile_over_time(0.5, m[5m]) / quantile_over_time(0.9, m[5m])",
                60_000,
                0.01,
            )],
            true,
        );
        for fixture in [shared_sum(), two_windows(), ratio] {
            let solution = solve(&fixture);
            let dag = decode(&fixture.physical, &solution);
            dag.validate(&fixture.physical).unwrap();
            let owners = solution
                .query_owners
                .iter()
                .chain(solution.instances.iter().map(|(_, _, owner)| owner));
            let uses: usize = owners.clone().map(|owner| owner.classes.len()).sum();
            let links: usize = owners.map(|owner| owner.links.len()).sum();
            assert_eq!(dag.candidate_of_node.len(), uses + solution.instances.len());
            assert_eq!(dag.edges.len(), links);
            // Back from the DAG: per candidate, how many runs.
            let mut runs: BTreeMap<PhysicalCandidateId, usize> = BTreeMap::new();
            for &candidate in &dag.candidate_of_node {
                *runs.entry(candidate).or_default() += 1;
            }
            let mut expected: BTreeMap<PhysicalCandidateId, usize> = BTreeMap::new();
            for owner in &solution.query_owners {
                for &class in &owner.classes {
                    *expected.entry(class).or_default() += 1;
                }
            }
            for (class, _, owner) in &solution.instances {
                *expected.entry(*class).or_default() += 1;
                for &used in &owner.classes {
                    *expected.entry(used).or_default() += 1;
                }
            }
            assert_eq!(runs, expected);
        }
    }

    // Two instances of one shareable candidate over different inputs are
    // two nodes, and two owners' uses of one stateless candidate are two
    // nodes.
    #[test]
    fn two_instances_and_two_uses_of_one_candidate_are_two_nodes_each() {
        let fixture = shared_sum();
        let (shared, below_a, below_b, top) = (
            PhysicalCandidateId(0),
            PhysicalCandidateId(1),
            PhysicalCandidateId(2),
            PhysicalCandidateId(3),
        );
        let reads = |index| ChosenOwner {
            classes: vec![top],
            links: vec![(
                top,
                0,
                ChosenProducer::Instance {
                    class: shared,
                    index,
                },
            )],
        };
        let below = |class| ChosenOwner {
            classes: vec![class],
            links: vec![(shared, 0, ChosenProducer::Stateless(class))],
        };
        let solution = SelectionMilpSolution {
            status: SelectionMilpStatus::Optimal,
            cost_per_second: 0.0,
            request_cost_per_second: 0.0,
            cost_lower_bound_per_second: 0.0,
            relative_gap: 0.0,
            outputs: vec![
                ChosenProducer::Stateless(top);
                output_references(&fixture.physical.query_roots).len()
            ],
            query_owners: vec![reads(0), reads(1)],
            instances: vec![(shared, 0, below(below_a)), (shared, 1, below(below_b))],
            size: SelectionMilpSize::default(),
            build_ms: 0,
            warm_start_ms: 0,
            solve_ms: 0,
        };
        let dag = decode(&fixture.physical, &solution);
        let count = |candidate| {
            dag.candidate_of_node
                .iter()
                .filter(|&&c| c == candidate)
                .count()
        };
        assert_eq!(
            (count(top), count(shared), count(below_a), count(below_b)),
            (2, 2, 1, 1)
        );
        assert_eq!(dag.edges.len(), 4);
        assert_ne!(
            dag.node_of_output_reference[0],
            dag.node_of_output_reference[1]
        );
    }
}
