//! Read a SQL aggregate from a finer aggregate's result
//! ([`match_finer`](crate::pass2::finer_aggregate::match_finer)): filter the finer
//! rows, then merge them into the query's groups with each measure's rollup.
use super::reuse::append_reader;
use crate::pass2::finer_aggregate::match_finer;
use crate::pass2::memo::candidates::{CandidateMemo, OriginOfCandidateForMemoGroup};
use crate::pass2::memo::check_generation_deadline;
use crate::pass2::memo::state_groups::StateGroupExpansionError;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, OperatorResultKind};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

/// A memo group's SQL aggregate: no `without`, measure filter or `HAVING`.
struct Aggregate<'a> {
    keys: &'a [usize],
    measures: &'a [AggIntent],
    output_names: &'a [String],
    rows: &'a Rc<OperatorNode>,
}

impl<'a> Aggregate<'a> {
    fn of(node: &'a OperatorNode) -> Option<Self> {
        if node.result_kind != OperatorResultKind::Relation {
            return None;
        }
        let NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(by),
            measures,
            output_names,
            filters,
            having: None,
            child,
        } = node.non_asap()?
        else {
            return None;
        };
        // A same-level read takes the one row of each group, so a measure
        // that gives several rows per group (a top-k, a count of values)
        // does not qualify.
        let one_row_per_group = !measures.iter().any(|measure| {
            matches!(
                measure,
                AggIntent::TopK { .. } | AggIntent::CountValues { .. }
            )
        });
        (!by.is_without() && filters.iter().all(Option::is_none) && one_row_per_group).then_some(
            Self {
                keys: by.keys(),
                measures,
                output_names,
                rows: child,
            },
        )
    }
}

/// The measure that merges finer values of `measure` (at `col` of the finer
/// result) into a coarser group (Calcite's `SqlAggFunction.getRollup`);
/// `None` when its values do not roll up. A count rolls up as a sum only
/// into grouped queries: a global count of no rows is 0, a sum NULL.
fn value_rollup(measure: &AggIntent, col: usize, global: bool) -> Option<AggIntent> {
    match measure {
        AggIntent::Sum { .. } => Some(AggIntent::Sum { col: Some(col) }),
        AggIntent::Count { .. } if !global => Some(AggIntent::Sum { col: Some(col) }),
        AggIntent::Min { .. } => Some(AggIntent::Min { col: Some(col) }),
        AggIntent::Max { .. } => Some(AggIntent::Max { col: Some(col) }),
        _ => None,
    }
}

pub(in crate::pass2::memo) fn add_finer_aggregate_candidates(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    // Only aggregates of equal measures can match; bucket by them (the
    // first levels of the paper's filter tree) instead of trying all pairs.
    let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
    for (group, entry) in memo.groups.iter().enumerate() {
        if let Some(aggregate) = Aggregate::of(&entry.logical_node) {
            buckets
                .entry(format!("{:?}", aggregate.measures))
                .or_default()
                .push(group);
        }
    }
    let mut added = 0;
    for groups in buckets.values() {
        for &owner in groups {
            for &finer in groups {
                check_generation_deadline(deadline)?;
                if owner != finer {
                    added += add_read(memo, owner, finer)?;
                }
            }
        }
    }
    Ok(added)
}

/// Adds to group `owner` a read of group `finer`'s result when it computes
/// `owner`'s aggregate; the number of candidates added (0 or 1).
fn add_read(
    memo: &mut CandidateMemo,
    owner: usize,
    finer: usize,
) -> Result<usize, StateGroupExpansionError> {
    let query_node = Rc::clone(&memo.groups[owner].logical_node);
    let finer_node = Rc::clone(&memo.groups[finer].logical_node);
    let (Some(query), Some(fine)) = (Aggregate::of(&query_node), Aggregate::of(&finer_node)) else {
        return Ok(0);
    };
    let Some(matched) = match_finer(query.rows, query.keys, fine.rows, fine.keys) else {
        return Ok(0);
    };
    let global = query.keys.is_empty();
    // A finer measure's value column follows the finer keys. When each query
    // group is at most one finer row, `MIN` reads that row's value, and over
    // no row gives a global query SQL's one NULL row.
    let measures = query
        .measures
        .iter()
        .enumerate()
        .map(|(index, measure)| {
            let col = fine.keys.len() + index;
            if !matched.needs_rollup && !(global && matches!(measure, AggIntent::Count { .. })) {
                Some(AggIntent::Min { col: Some(col) })
            } else if matched.needs_rollup {
                value_rollup(measure, col, global)
            } else {
                None
            }
        })
        .collect::<Option<Vec<_>>>();
    let Some(measures) = measures else {
        return Ok(0);
    };
    let reuse_error = |error: asap_types::ir::SchemaDerivationError| {
        StateGroupExpansionError::ReuseRule(error.to_string())
    };
    let rows = match matched.compensating_filter {
        Some(pred) => OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
            pred,
            child: Rc::clone(&finer_node),
        }))
        .map_err(reuse_error)?,
        None => Rc::clone(&finer_node),
    };
    let reader = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::by(matched.query_keys),
        measures,
        output_names: query.output_names.to_vec(),
        filters: vec![],
        having: None,
        child: rows,
    }))
    .map_err(reuse_error)?;
    let Some(original) = memo.candidates[owner]
        .iter()
        .find(|c| c.origin == OriginOfCandidateForMemoGroup::Pass1)
        .cloned()
    else {
        return Ok(0);
    };
    // The reader's columns take the query's qualifiers. A rollup whose type
    // or nullability differs from the query's (a sum of counts) is not
    // offered.
    let mut schema = reader.schema.clone();
    for (field, expected) in schema
        .fields
        .iter_mut()
        .zip(&original.root_node.schema.fields)
    {
        field.table = expected.table.clone();
    }
    if schema != original.root_node.schema || reader.result_kind != original.root_node.result_kind {
        return Ok(0);
    }
    let reader = Rc::new(OperatorNode::with_schema(reader.operator.clone(), schema));
    Ok(usize::from(append_reader(
        memo,
        owner,
        &original,
        reader,
        OriginOfCandidateForMemoGroup::Rollup,
    )?))
}
