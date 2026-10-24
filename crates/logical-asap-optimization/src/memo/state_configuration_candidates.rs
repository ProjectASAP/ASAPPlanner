//! Expand compatible readers at finite shared sizes and pane horizons before
//! interning concrete states. Local candidates remain available.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::schema::{sketch::SketchKind, FieldDataType};
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};

use super::candidates::CandidateMemo;
use super::shared_builds::{size_thresholds_for, with_child_nodes};
use super::state_configurations::{pane_configurations, shared_size, PaneRequirement};
use super::state_groups::{same_range_input, simple_build, StateGroupExpansionError};
use crate::asap_aware_cse::{with_pane_chain, with_resized_build};

struct Reader {
    group: usize,
    candidate: usize,
    build: Rc<OperatorNode>,
    merge: Option<Rc<OperatorNode>>,
    family: FieldDataType,
}

pub(super) fn add_state_configuration_candidates(
    memo: &mut CandidateMemo,
) -> Result<(), StateGroupExpansionError> {
    let mut buckets: Vec<Vec<Reader>> = Vec::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (candidate, entry) in candidates.iter().enumerate() {
            let Some((build, _)) = simple_build(memo, entry) else {
                continue;
            };
            // Configuration expansion precedes Stage 2. The rewrite helpers
            // create unassigned state; preserve any already assigned build.
            if build.timing.is_some()
                || build.materialization != asap_types::physical::Materialization::NotMaterialized
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
                candidate,
                build,
                merge,
                family,
            };
            if let Some(bucket) = buckets
                .iter_mut()
                .find(|bucket| compatible(&bucket[0], &reader))
            {
                bucket.push(reader);
            } else {
                buckets.push(vec![reader]);
            }
        }
    }
    for readers in buckets {
        if !readers
            .iter()
            .any(|reader| reader.group != readers[0].group)
        {
            continue;
        }
        let pane_requirements: Option<Vec<_>> = readers.iter().map(pane_requirement).collect();
        let pane_configurations = pane_requirements
            .as_ref()
            .map(|requirements| pane_configurations(requirements));
        let families = if pane_configurations.is_none() {
            let requirements = readers
                .iter()
                .map(|reader| match &reader.family {
                    FieldDataType::Sketch(kind, _) => Some(kind.params().clone()),
                    _ => None,
                })
                .collect();
            size_thresholds_for(requirements)
                .into_iter()
                .map(
                    |configuration| match (&readers[0].family, configuration.params) {
                        (FieldDataType::Sketch(kind, grouping), Some(params)) => {
                            FieldDataType::Sketch(
                                SketchKind::new(kind.algorithm().clone(), params),
                                grouping.clone(),
                            )
                        }
                        _ => readers[0].family.clone(),
                    },
                )
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for reader in &readers {
            let original = memo.candidates[reader.group][reader.candidate].clone();
            let roots = [QueryRoot::Operator(Rc::clone(&original.node))];
            let mut rewritten = Vec::new();
            if let Some(configurations) = &pane_configurations {
                let requirement = pane_requirement(reader).expect("pane bucket");
                for configuration in configurations {
                    if !configuration.serves(&requirement) {
                        continue;
                    }
                    let Ok(count) = u32::try_from(
                        configuration.longest_window_ms / configuration.pane_width_ms,
                    ) else {
                        continue;
                    };
                    rewritten.push(with_pane_chain(
                        &roots,
                        reader.merge.as_ref().unwrap(),
                        configuration.family.clone(),
                        configuration.pane_width_ms,
                        count,
                    ));
                }
            } else {
                for family in &families {
                    if !shared_size(family, &reader.family)
                        .is_some_and(|joined| same_family(&joined, family))
                    {
                        continue;
                    }
                    rewritten.push(with_resized_build(&roots, &reader.build, family.clone()));
                }
            }
            let boundaries: HashSet<_> = original
                .children
                .iter()
                .map(|group| Rc::as_ptr(&memo.groups[group.0 as usize].node))
                .collect();
            for result in rewritten {
                let roots = result.map_err(|error| {
                    StateGroupExpansionError::StateConfiguration(error.to_string())
                })?;
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
                if memo.candidates[reader.group].iter().any(|known| {
                    *known.node == *node
                        && serde_json::to_string(&known.node).ok()
                            == serde_json::to_string(&node).ok()
                }) {
                    continue;
                }
                let mut candidate = original.clone();
                candidate.node = node;
                memo.candidates[reader.group].push(candidate);
            }
        }
    }
    Ok(())
}

fn pane_requirement(reader: &Reader) -> Option<PaneRequirement> {
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

fn compatible(left: &Reader, right: &Reader) -> bool {
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
    let signature = |reader: &Reader| {
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
