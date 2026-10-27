//! Read one existing grouped quantile instead of rebuilding its filtered input.
use super::reuse::append_reader;
use crate::memo::candidates::{CandidateMemo, CandidateOrigin};
use crate::memo::check_generation_deadline;
use crate::memo::state_groups::StateGroupExpansionError;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::scalar::CompareOpKind;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr};
use std::rc::Rc;
use std::time::Instant;

// Removing exactly one key equality must leave the identical source and window.
fn extra_key_filter(a: &Rc<OperatorNode>, b: &Rc<OperatorNode>, key: usize) -> Option<Predicate> {
    match (a.non_asap()?, b.non_asap()?) {
        (
            NonASAPOp::TimeRange { range, kind, child },
            NonASAPOp::TimeRange {
                range: r,
                kind: k,
                child: other,
            },
        ) if range == r && kind == k => extra_key_filter(child, other, key),
        (
            NonASAPOp::Scan {
                source,
                schema,
                predicates,
            },
            NonASAPOp::Scan {
                source: other,
                schema: s,
                predicates: base,
            },
        ) if source == other && schema == s && predicates.len() == base.len() + 1 => {
            for (index, pred) in predicates.iter().enumerate() {
                if predicates
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != index)
                    .map(|(_, p)| p)
                    .eq(base.iter())
                    && matches!(&pred.0, ScalarExpr::Compare { left, op: CompareOpKind::Eq, right, .. }
                            if matches!(&**left, ScalarExpr::Column(i) if *i == key) && matches!(&**right, ScalarExpr::Literal(_)))
                {
                    return Some(Predicate(pred.0.map_columns(&mut |_| 0)));
                }
            }
            None
        }
        _ => None,
    }
}

pub(in crate::memo) fn add_filtered_group_candidates(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    let groups = memo.groups.clone();
    let mut added = 0;
    for (owner, coarse) in groups.iter().enumerate() {
        check_generation_deadline(deadline)?;
        if coarse.node.result_kind != asap_types::ir::OperatorResultKind::Relation {
            continue;
        }
        let Some(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(by),
            measures,
            filters,
            having: None,
            child,
            ..
        }) = coarse.node.non_asap()
        else {
            continue;
        };
        if by.is_without()
            || !by.is_empty()
            || !filters.iter().all(Option::is_none)
            || !matches!(measures.as_slice(), [AggIntent::Quantile { .. }])
        {
            continue;
        }
        for fine in &groups {
            check_generation_deadline(deadline)?;
            let Some(NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(keys),
                measures: other,
                filters: other_filters,
                having: None,
                child: source,
                ..
            }) = fine.node.non_asap()
            else {
                continue;
            };
            if keys.is_without()
                || keys.len() != 1
                || measures != other
                || !other_filters.iter().all(Option::is_none)
            {
                continue;
            }
            let Some(pred) = extra_key_filter(child, source, keys.keys()[0]) else {
                continue;
            };
            let filtered = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
                pred,
                child: Rc::clone(&fine.node),
            }))
            .map_err(|e| StateGroupExpansionError::ReuseRule(e.to_string()))?;
            // SQL's global quantile returns one NULL row on an empty input.
            // MIN over the selected (at most one) group preserves that contract.
            let reader = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::Min { col: Some(1) }],
                output_names: vec![coarse.node.schema.fields[0].name.clone()],
                filters: vec![],
                having: None,
                child: filtered,
            }))
            .map_err(|e| StateGroupExpansionError::ReuseRule(e.to_string()))?;
            let Some(original) = memo.candidates[owner]
                .iter()
                .find(|c| c.origin == CandidateOrigin::Pass1)
                .cloned()
            else {
                continue;
            };
            added += usize::from(append_reader(
                memo,
                owner,
                &original,
                reader,
                CandidateOrigin::Rollup,
            )?);
        }
    }
    Ok(added)
}
