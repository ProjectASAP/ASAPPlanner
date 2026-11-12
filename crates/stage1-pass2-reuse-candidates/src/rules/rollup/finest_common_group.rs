//! A cluster's finest common group $G^*$, which every member can roll up
//! from.

use std::collections::{BTreeMap, BTreeSet};

use crate::rules::candidates::add_candidate_if_output_matches;
use crate::Pass2RuleError;
use asap_types::ir::operator::arguments::SourceColumn;
use asap_types::ir::operator::Reduction;
use asap_types::ir::properties::selection::Constraint;
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{
    listed_build_parameters, Memo, MemoGroupId, OriginOfCandidateForMemoGroup,
};

use super::member::{column_of, Member};

/// The cluster's finest common group $G^*$: its selection is, per column,
/// the union of the members' value-set constraints (none where a member
/// has none); its keys are every member's keys plus each column on which a
/// member selects fewer rows than $G^*$. It is a member when one has that
/// selection and those keys; otherwise it is built over the rows of a
/// member whose selection is the union, with one build per listed θ.
/// `None` when no member's rows have the union selection (a new scan of
/// the union is not built) or a key is not a column of those rows.
pub(super) fn finest_common_group(
    memo: &mut Memo,
    cluster: &[Member],
) -> Result<Option<MemoGroupId>, Pass2RuleError> {
    let columns: BTreeSet<&SourceColumn> = cluster
        .iter()
        .flat_map(|member| member.selection.keys())
        .collect();
    let mut hull = BTreeMap::new();
    let mut keys: BTreeSet<SourceColumn> = cluster
        .iter()
        .flat_map(|member| member.keys.iter().cloned())
        .collect();
    for column in columns {
        let constraints: Option<Vec<&Constraint>> = cluster
            .iter()
            .map(|member| member.selection.get(column))
            .collect();
        let union = constraints.and_then(|constraints| {
            if constraints
                .iter()
                .all(|constraint| *constraint == constraints[0])
            {
                return Some(constraints[0].clone());
            }
            let mut values: Vec<ScalarValue> = Vec::new();
            for constraint in constraints {
                let Constraint::In(more) = constraint else {
                    return None;
                };
                values.extend(
                    more.iter()
                        .filter(|value| !values.contains(value))
                        .cloned()
                        .collect::<Vec<_>>(),
                );
            }
            Some(Constraint::In(values))
        });
        if let Some(union) = union {
            hull.insert(column.clone(), union);
        }
        if cluster
            .iter()
            .any(|member| member.selection.get(column) != hull.get(column))
        {
            keys.insert(column.clone());
        }
    }
    if let Some(member) = cluster.iter().find(|member| {
        member.selection == hull && member.keys.iter().cloned().collect::<BTreeSet<_>>() == keys
    }) {
        return Ok(Some(member.group));
    }
    let Some(base) = cluster.iter().find(|member| member.selection == hull) else {
        return Ok(None);
    };
    let rows_lineage = &memo.groups()[base.rows.0 as usize]
        .group_properties
        .column_lineage;
    let mut key_columns = Vec::new();
    for key in &keys {
        let Some(column) = column_of(rows_lineage, key) else {
            return Ok(None);
        };
        key_columns.push(column);
    }
    key_columns.sort_unstable();
    let mut expression = base.expression.clone();
    match &mut expression {
        Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. })
        | Operator::NonASAP(NonASAPOp::Aggregate { reduction, .. }) => {
            *reduction = Reduction::by(key_columns)
        }
        _ => unreachable!("a member is a SummaryAgg or an Aggregate"),
    }
    let finest = memo.find_or_add_group(expression.clone())?;
    for theta in listed_build_parameters(&expression) {
        add_candidate_if_output_matches(
            memo,
            finest,
            expression.clone(),
            theta,
            OriginOfCandidateForMemoGroup::Rollup,
        )?;
    }
    Ok(Some(finest))
}
