//! Expand compatible readers at finite shared sizes and pane horizons before
//! interning concrete states. Local candidates remain available.
use crate::pass2::memo::check_generation_deadline;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use asap_types::ir::flat::{flatten, FlatDag};
use asap_types::ir::schema::{sketch::SketchKind, FieldDataType};
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};

use super::shared_producer::{
    add_shared_producer_candidates, spans_groups, Reader, SharedProducer,
};
use crate::pass2::asap_aware_cse::{with_pane_chain, with_resized_build};
use crate::pass2::memo::candidates::CandidateMemo;
use crate::pass2::memo::shared_configurations::{
    pane_configurations_before, shared_size, state_size_configurations, PaneConfiguration,
    PaneRequirement, PaneWidthDomain,
};
use crate::pass2::memo::state_groups::{same_range_input, simple_build, StateGroupExpansionError};

/// `resized` (a rewrite of `original` with the same shape) with each node
/// that is a child group's node in `original` put back, and the nodes above
/// rebuilt over them, keeping their own schema and guarantee.
fn with_child_nodes(
    original: &Rc<OperatorNode>,
    resized: &Rc<OperatorNode>,
    child_nodes: &HashSet<*const OperatorNode>,
    done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    if child_nodes.contains(&Rc::as_ptr(original)) {
        return Rc::clone(original);
    }
    if let Some(found) = done.get(&Rc::as_ptr(resized)) {
        return Rc::clone(found);
    }
    let pairs: HashMap<*const OperatorNode, Rc<OperatorNode>> = original
        .children()
        .into_iter()
        .zip(resized.children())
        .map(|(o, r)| (Rc::as_ptr(r), with_child_nodes(o, r, child_nodes, done)))
        .collect();
    let mut changed = false;
    let operator = resized.operator.map_children(|child| {
        let new = Rc::clone(&pairs[&Rc::as_ptr(child)]);
        changed |= !Rc::ptr_eq(&new, child);
        new
    });
    let out = match changed {
        false => Rc::clone(resized),
        true => Rc::new(
            OperatorNode::with_schema(operator, resized.schema.clone())
                .with_guarantee(resized.guarantee.clone())
                .with_timing(resized.timing)
                .with_materialization(resized.materialization),
        ),
    };
    done.insert(Rc::as_ptr(resized), Rc::clone(&out));
    out
}

/// A summary build one candidate reads: its own need of the shared producer.
struct StateNeed {
    candidate: usize,
    build: Rc<OperatorNode>,
    merge: Option<Rc<OperatorNode>>,
    family: FieldDataType,
    /// The reader's normalized pane layout, when its set shares panes: equal
    /// layouts of one set have one index, so a configuration rewrites them once.
    normalized_layout: Option<usize>,
}

/// A shared build's need: one pane layout (its index among the set's pane
/// configurations, for request deduplication) or one size.
enum StateCovering {
    Panes {
        configuration_index: usize,
        configuration: PaneConfiguration,
    },
    Size(FieldDataType),
}

/// Summary builds of one family over one input at different sizes or pane
/// layouts: each reader is rewritten to a build at each covering size or
/// layout that serves it, and equal concrete builds become one state group
/// later ([`super::super::state_groups`]).
struct SummaryStates {
    deduplicate_requests: bool,
    domain: PaneWidthDomain,
    /// Every candidate's graph signature by group, so no rewrite repeats one.
    known: HashMap<(usize, String), Vec<Rc<OperatorNode>>>,
    rewrite_count: usize,
    /// The (normalized layout, configuration index) rewrites already made.
    requests: HashSet<(usize, usize)>,
}

pub(in crate::pass2::memo) fn add_state_configuration_candidates_in_domain(
    memo: &mut CandidateMemo,
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<(), StateGroupExpansionError> {
    expand_configurations(memo, true, domain, deadline).map(|_| ())
}

fn expand_configurations(
    memo: &mut CandidateMemo,
    deduplicate_requests: bool,
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    let mut states = SummaryStates {
        deduplicate_requests,
        domain,
        known: HashMap::new(),
        rewrite_count: 0,
        requests: HashSet::new(),
    };
    add_shared_producer_candidates(memo, &mut states, deadline)?;
    Ok(states.rewrite_count)
}

impl SharedProducer for SummaryStates {
    type Need = StateNeed;
    type Covering = StateCovering;

    fn sets(
        &mut self,
        memo: &CandidateMemo,
        deadline: Option<Instant>,
    ) -> Result<Vec<Vec<Reader<StateNeed>>>, StateGroupExpansionError> {
        let mut sets: Vec<Vec<Reader<StateNeed>>> = Vec::new();
        for (group, candidates) in memo.candidates.iter().enumerate() {
            check_generation_deadline(deadline)?;
            for (candidate, entry) in candidates.iter().enumerate() {
                check_generation_deadline(deadline)?;
                let Some((build, _)) = simple_build(memo, entry) else {
                    continue;
                };
                // Configuration expansion precedes Stage 2. The rewrite helpers
                // create unassigned state; preserve any already assigned build.
                if build.timing.is_some()
                    || build.materialization
                        != asap_types::physical::Materialization::NotMaterialized
                {
                    continue;
                }
                let family = match build.asap() {
                    Some(ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. }) => {
                        family.clone()
                    }
                    _ => unreachable!(),
                };
                let merge = if matches!(build.asap(), Some(ASAPOp::PaneChain { .. })) {
                    let merges: Vec<_> = OperatorNode::reachable(&entry.node).into_iter().filter(|node| {
                        matches!(node.asap(), Some(ASAPOp::SummaryMerge {children, group_by: None, ..})
                            if children.len() == 1 && Rc::ptr_eq(&children[0], &build))
                    }).collect();
                    // The existing rewrite changes one plain merge. Other pane uses
                    // need their own explicit transformation, not guessed semantics.
                    if merges.len() != 1 {
                        continue;
                    }
                    Some(Rc::clone(&merges[0]))
                } else {
                    None
                };
                let reader = Reader {
                    group,
                    need: StateNeed {
                        candidate,
                        build,
                        merge,
                        family,
                        normalized_layout: None,
                    },
                };
                match sets
                    .iter_mut()
                    .find(|set| compatible(&set[0].need, &reader.need))
                {
                    Some(set) => set.push(reader),
                    None => sets.push(vec![reader]),
                }
            }
        }
        for (group, candidates) in memo.candidates.iter().enumerate() {
            check_generation_deadline(deadline)?;
            for candidate in candidates {
                check_generation_deadline(deadline)?;
                self.known
                    .entry((group, graph_signature(&candidate.node)?))
                    .or_default()
                    .push(Rc::clone(&candidate.node));
            }
        }
        let mut layouts: Vec<(usize, usize, FlatDag)> = Vec::new();
        for set in &mut sets {
            if spans_groups(set) {
                assign_normalized_layouts(memo, set, &mut layouts, deadline)?;
            }
        }
        Ok(sets)
    }

    fn coverings(
        &self,
        set: &[Reader<StateNeed>],
        deadline: Option<Instant>,
    ) -> Result<Vec<StateCovering>, StateGroupExpansionError> {
        let pane_requirements: Option<Vec<_>> = set
            .iter()
            .map(|reader| pane_requirement(&reader.need))
            .collect();
        let Some(requirements) = pane_requirements else {
            let requirements = set
                .iter()
                .map(|reader| match &reader.need.family {
                    FieldDataType::Sketch(kind, _) => Some(kind.params().clone()),
                    _ => None,
                })
                .collect();
            let first = &set[0].need.family;
            return Ok(state_size_configurations(requirements, deadline)?
                .into_iter()
                .map(|configuration| {
                    StateCovering::Size(match (first, configuration) {
                        (FieldDataType::Sketch(kind, grouping), Some(params)) => {
                            FieldDataType::Sketch(
                                SketchKind::new(*kind.algorithm(), params),
                                grouping.clone(),
                            )
                        }
                        _ => first.clone(),
                    })
                })
                .collect());
        };
        Ok(
            pane_configurations_before(&requirements, self.domain, deadline)?
                .into_iter()
                .enumerate()
                .map(
                    |(configuration_index, configuration)| StateCovering::Panes {
                        configuration_index,
                        configuration,
                    },
                )
                .collect(),
        )
    }

    fn derive(
        &mut self,
        memo: &mut CandidateMemo,
        reader: &Reader<StateNeed>,
        covering: &StateCovering,
    ) -> Result<usize, StateGroupExpansionError> {
        let need = &reader.need;
        let serves = match covering {
            StateCovering::Panes { configuration, .. } => {
                configuration.serves(&pane_requirement(need).expect("pane set"))
            }
            StateCovering::Size(family) => {
                shared_size(family, &need.family).is_some_and(|joined| same_family(&joined, family))
            }
        };
        if !serves {
            return Ok(0);
        }
        let original = memo.candidates[reader.group][need.candidate].clone();
        let roots = [QueryRoot::Operator(Rc::clone(&original.node))];
        let result = match covering {
            StateCovering::Panes {
                configuration_index,
                configuration,
            } => {
                let Ok(count) =
                    u32::try_from(configuration.longest_window_ms / configuration.pane_width_ms)
                else {
                    return Ok(0);
                };
                if self.deduplicate_requests
                    && need
                        .normalized_layout
                        .is_some_and(|layout| !self.requests.insert((layout, *configuration_index)))
                {
                    return Ok(0);
                }
                self.rewrite_count += 1;
                with_pane_chain(
                    &roots,
                    need.merge.as_ref().unwrap(),
                    configuration.family.clone(),
                    configuration.pane_width_ms,
                    count,
                )
            }
            StateCovering::Size(family) => {
                self.rewrite_count += 1;
                with_resized_build(&roots, &need.build, family.clone())
            }
        };
        let boundaries: HashSet<_> = original
            .children
            .iter()
            .map(|group| Rc::as_ptr(&memo.groups[group.0 as usize].node))
            .collect();
        let roots = result
            .map_err(|error| StateGroupExpansionError::StateConfiguration(error.to_string()))?;
        let [QueryRoot::Operator(node)] = roots.as_slice() else {
            unreachable!("operator rewrite")
        };
        let node = with_child_nodes(&original.node, node, &boundaries, &mut HashMap::new());
        // Resizing affects internal state types, never public output fields.
        if original.node.schema.fields != node.schema.fields
            || original.node.schema.time_index != node.schema.time_index
            || original.node.schema.unique_keys != node.schema.unique_keys
            || (original.node.schema.closed && !node.schema.closed)
        {
            return Err(StateGroupExpansionError::StateConfiguration(
                "state rewrite changed the reader output contract".into(),
            ));
        }
        let previous = self
            .known
            .entry((reader.group, graph_signature(&node)?))
            .or_default();
        if previous.iter().any(|known| **known == *node) {
            return Ok(0);
        }
        previous.push(Rc::clone(&node));
        let mut candidate = original;
        candidate.node = node;
        memo.candidates[reader.group].push(candidate);
        Ok(1)
    }
}

/// Numbers the normalized pane layouts of `set`'s readers, when it shares
/// panes: different original layouts can produce the same requested
/// reader/configuration, so each is normalized once and its rewrite made once,
/// instead of once for every existing pane width. Numbers continue across
/// sets (`layouts`), so a configuration index of one set never matches
/// another's.
fn assign_normalized_layouts(
    memo: &CandidateMemo,
    set: &mut [Reader<StateNeed>],
    layouts: &mut Vec<(usize, usize, FlatDag)>,
    deadline: Option<Instant>,
) -> Result<(), StateGroupExpansionError> {
    let Some(requirements) = set
        .iter()
        .map(|reader| pane_requirement(&reader.need))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(());
    };
    let mut layout_keys: HashMap<(usize, String), Vec<usize>> = HashMap::new();
    for (reader, requirement) in set.iter_mut().zip(&requirements) {
        check_generation_deadline(deadline)?;
        let Ok(count) = u32::try_from(requirement.window_ms) else {
            // Normalization must not narrow the legal long-window domain.
            continue;
        };
        let need = &reader.need;
        let original = &memo.candidates[reader.group][need.candidate];
        let roots = [QueryRoot::Operator(Rc::clone(&original.node))];
        let normalized = with_pane_chain(
            &roots,
            need.merge.as_ref().unwrap(),
            need.family.clone(),
            1,
            count,
        )
        .map_err(|error| StateGroupExpansionError::StateConfiguration(error.to_string()))?;
        let flat = flatten(&normalized).0;
        let signature = serde_json::to_string(&flat)
            .map_err(|error| StateGroupExpansionError::StateConfiguration(error.to_string()))?;
        let indices = layout_keys.entry((reader.group, signature)).or_default();
        let existing = indices.iter().copied().find(|&index| {
            let (group, candidate, previous) = &layouts[index];
            let previous_candidate = &memo.candidates[*group][*candidate];
            previous == &flat
                && previous_candidate.origin == original.origin
                && previous_candidate.children == original.children
                && previous_candidate.implements == original.implements
                && previous_candidate.budget_share == original.budget_share
        });
        reader.need.normalized_layout = Some(existing.unwrap_or_else(|| {
            let index = layouts.len();
            layouts.push((reader.group, need.candidate, flat));
            indices.push(index);
            index
        }));
    }
    Ok(())
}

fn graph_signature(node: &Rc<OperatorNode>) -> Result<String, StateGroupExpansionError> {
    serde_json::to_string(&flatten(&[QueryRoot::Operator(Rc::clone(node))]).0)
        .map_err(|error| StateGroupExpansionError::StateConfiguration(error.to_string()))
}

fn pane_requirement(reader: &StateNeed) -> Option<PaneRequirement> {
    let ASAPOp::PaneChain {
        pane_width_ms,
        pane_count,
        ..
    } = reader.build.asap()?
    else {
        return None;
    };
    let ASAPOp::SummaryMerge { newest_panes, .. } = reader.merge.as_ref()?.asap()? else {
        return None;
    };
    Some(PaneRequirement {
        pane_width_ms: *pane_width_ms,
        window_ms: pane_width_ms.checked_mul(u64::from(newest_panes.unwrap_or(*pane_count)))?,
        family: reader.family.clone(),
    })
}

fn same_family(left: &FieldDataType, right: &FieldDataType) -> bool {
    left == right && serde_json::to_string(left).ok() == serde_json::to_string(right).ok()
}

fn compatible(left: &StateNeed, right: &StateNeed) -> bool {
    if shared_size(&left.family, &right.family).is_none() {
        return false;
    }
    let left_inputs = left.build.children();
    let right_inputs = right.build.children();
    if left_inputs.len() != right_inputs.len()
        || !left_inputs
            .iter()
            .zip(right_inputs)
            .all(|(a, b)| Rc::ptr_eq(a, b) || same_range_input(a, b))
    {
        return false;
    }
    // Ignore only the fields this rule rewrites. Build guarantees are derived
    // from these operators and are recomposed by the existing rewrite helpers.
    let signature = |reader: &StateNeed| {
        let mut operator = reader.build.operator.map_children(|_| ());
        match &mut operator {
            Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) => *family = left.family.clone(),
            Operator::ASAP(ASAPOp::PaneChain {
                family,
                pane_width_ms,
                pane_count,
                ..
            }) => {
                *family = left.family.clone();
                *pane_width_ms = 1;
                *pane_count = 1;
            }
            _ => unreachable!(),
        }
        let mut schema = reader.build.schema.clone();
        for field in &mut schema.fields {
            if same_family(&field.dtype, &reader.family) {
                field.dtype = left.family.clone();
            }
        }
        serde_json::to_string(&(
            operator,
            schema,
            reader.build.result_kind,
            reader.build.timing,
            reader.build.materialization,
        ))
        .ok()
    };
    signature(left).is_some_and(|signature_left| Some(signature_left) == signature(right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use crate::pass2::memo::{candidates::try_populate_pass1, DagOfLogicalMemoGroups};
    use crate::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};

    // Request deduplication preserves the entire declared finite configuration
    // set while avoiding repeated rewrites through every initial pane layout.
    #[test]
    fn equivalent_pane_layouts_expand_each_configuration_once() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let roots: Vec<_> = [0.95, 0.99]
            .into_iter()
            .enumerate()
            .map(|(query, quantile)| {
                (
                    query,
                    QueryRoot::Operator(lower_promql(
                        &format!("quantile_over_time({quantile}, latency[5m])"),
                        target.clone(),
                    )),
                )
            })
            .collect();
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let inventory = generate_per_computation_candidates(roots).unwrap();
        let mut memo = try_populate_pass1(initial, &inventory).unwrap();
        let entries = vec![
            entry_with(
                target,
                QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(6)))
            );
            2
        ];
        crate::pass2::memo::window_candidates::add_local_window_composition(&mut memo, &entries);
        let mut unoptimized = memo.clone();
        let optimized_rewrites =
            expand_configurations(&mut memo, true, PaneWidthDomain::Complete, None).unwrap();
        let exhaustive_rewrites =
            expand_configurations(&mut unoptimized, false, PaneWidthDomain::Complete, None)
                .unwrap();
        assert!(
            optimized_rewrites < exhaustive_rewrites,
            "{optimized_rewrites} versus {exhaustive_rewrites}"
        );
        let signatures = |memo: &CandidateMemo| -> Vec<HashSet<String>> {
            memo.candidates
                .iter()
                .map(|candidates| {
                    candidates
                        .iter()
                        .map(|candidate| graph_signature(&candidate.node).unwrap())
                        .collect()
                })
                .collect()
        };
        assert_eq!(signatures(&memo), signatures(&unoptimized));
        let widths: HashSet<_> = memo
            .candidates
            .iter()
            .flatten()
            .flat_map(|candidate| OperatorNode::reachable(&candidate.node))
            .filter_map(|node| match node.asap() {
                Some(ASAPOp::PaneChain { pane_width_ms, .. }) => Some(*pane_width_ms),
                _ => None,
            })
            .collect();
        assert_eq!(widths, HashSet::from([1, 2, 3, 6]));
    }
}
