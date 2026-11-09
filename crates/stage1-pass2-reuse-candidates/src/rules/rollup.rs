//! Step 1, rollup from the finest common group (concepts doc, section 3):
//! a coarse aggregate group $G_c$ computed from a finer group $G_f$ of the
//! same definition, and per cluster of equal definitions one finest common
//! group $G^*$ every member can roll up from.

use std::collections::{BTreeMap, BTreeSet};

use crate::rules::{
    add_candidate, add_candidates_per_input_parameters, add_group_computing_itself, Pass2RuleError,
};
use asap_types::ir::operator::operator_properties::SourceColumn;
use asap_types::ir::operator::{AggIntent, Reduction};
use asap_types::ir::properties::summary_coverage::{Constraint, SummaryStateDefinition};
use asap_types::ir::scalar::{CompareOpKind, ExprSemantics, Predicate, ScalarExpr, ScalarValue};
use asap_types::ir::schema::{FieldDataType, SummaryTypeCapability};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorResultKind};
use asap_types::stages::{
    parameter_list, ColumnLineage, Memo, MemoGroupId, OriginOfCandidateForMemoGroup,
};

/// Adds, to each aggregate group $G_c$, a candidate computing it from each
/// finer group $G_f$ of its cluster ([`rollup_from`]), after synthesizing
/// each cluster's finest common group ([`finest_common_group`]). A cluster
/// is the summary state groups (`SummaryAgg` of a summary type whose
/// states merge across groups) or exact `Aggregate` groups (of sums,
/// counts, minima and maxima) with equal definitions apart from their keys
/// ([`Member::definition`]) and equal interval constraints (the time
/// window).
pub(crate) fn add_rollup_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let mut clusters: Vec<Vec<Member>> = Vec::new();
    for index in 0..memo.groups().len() {
        let Some(member) = Member::of(memo, MemoGroupId(index as u32)) else {
            continue;
        };
        match clusters
            .iter_mut()
            .find(|cluster| cluster[0].same_cluster(&member))
        {
            Some(cluster) => cluster.push(member),
            None => clusters.push(vec![member]),
        }
    }
    for mut cluster in clusters.into_iter().filter(|cluster| cluster.len() > 1) {
        if let Some(finest) = finest_common_group(memo, &cluster)? {
            if cluster.iter().all(|member| member.group != finest) {
                cluster.extend(Member::of(memo, finest));
            }
        }
        for coarse in &cluster {
            for fine in &cluster {
                if coarse.group != fine.group {
                    rollup_from(memo, coarse, fine)?;
                }
            }
        }
    }
    Ok(())
}

/// One aggregate group as a rollup rule reads it.
#[derive(Debug, Clone)]
struct Member {
    group: MemoGroupId,
    /// The group's expression.
    expression: Operator<MemoGroupId>,
    /// The rows it aggregates: the `SummaryAgg`'s or `Aggregate`'s input.
    rows: MemoGroupId,
    /// Its group-by keys, in output order, by the source column they copy.
    keys: Vec<SourceColumn>,
    /// Its coverage's selection, one box.
    selection: BTreeMap<SourceColumn, Constraint>,
    /// Its coverage's definition without its keys: the top operator's
    /// reduction (and an `Aggregate`'s output names) blanked, and an input
    /// group that is a `Scan` written as that `Scan` (as when one of its
    /// predicates is lifted), so equal computations over one source compare
    /// equal whatever their keys and label selections.
    definition: SummaryStateDefinition,
}

impl Member {
    fn of(memo: &Memo, group: MemoGroupId) -> Option<Self> {
        let memo_group = &memo.groups()[group.0 as usize];
        let (rows, by) = match &memo_group.expression {
            Operator::ASAP(ASAPOp::SummaryAgg {
                child,
                summary_type,
                reduction: Reduction::Reduce(by),
                ..
            }) if SummaryTypeCapability::merges_groups(summary_type) => (*child, by),
            Operator::NonASAP(NonASAPOp::Aggregate {
                child,
                reduction: Reduction::Reduce(by),
                measures,
                filters,
                having: None,
                ..
            }) if filters.iter().all(Option::is_none)
                && measures
                    .iter()
                    .all(|measure| rolled_up(measure, 0).is_some()) =>
            {
                (*child, by)
            }
            _ => return None,
        };
        if by.is_without() {
            return None;
        }
        let coverage = memo_group.group_properties.aggregate_coverage.as_ref()?;
        let [selection] = coverage.selection() else {
            return None;
        };
        let rows_lineage = &memo.groups()[rows.0 as usize]
            .group_properties
            .column_lineage;
        let keys = by
            .keys()
            .iter()
            .map(|&key| match rows_lineage.get(key)? {
                ColumnLineage::SourceColumn(column) => Some(column.clone()),
                ColumnLineage::Computed => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let mut definition = coverage.definition().clone();
        match definition.unlifted_operators.first_mut()? {
            Operator::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => {
                *reduction = Reduction::PerEntity
            }
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction,
                output_names,
                ..
            }) => {
                *reduction = Reduction::PerEntity;
                output_names.clear();
            }
            _ => return None,
        }
        if let Some(input) = definition.input_group {
            if let scan @ Operator::NonASAP(NonASAPOp::Scan { .. }) =
                &memo.groups()[input.0 as usize].expression
            {
                definition.input_group = None;
                definition
                    .unlifted_operators
                    .push(scan.map_children(|&child| Some(child)));
            }
        }
        Some(Self {
            group,
            expression: memo_group.expression.clone(),
            rows,
            keys,
            selection: selection.columns().clone(),
            definition,
        })
    }

    /// Whether `other` computes the same thing over the same time window:
    /// equal definitions without keys and equal interval constraints.
    fn same_cluster(&self, other: &Self) -> bool {
        self.definition == other.definition && self.intervals() == other.intervals()
    }

    /// Its interval constraints (the time window, value ranges): equal
    /// across a cluster, never compensated.
    fn intervals(&self) -> BTreeMap<&SourceColumn, &Constraint> {
        self.selection
            .iter()
            .filter(|(_, constraint)| matches!(constraint, Constraint::Interval { .. }))
            .collect()
    }
}

/// The cluster's finest common group $G^*$: its selection is, per column,
/// the union of the members' value-set constraints (none where a member
/// has none); its keys are every member's keys plus each column on which a
/// member selects fewer rows than $G^*$. It is a member when one has that
/// selection and those keys; otherwise it is built over the rows of a
/// member whose selection is the union, with one build per listed θ.
/// `None` when no member's rows have the union selection (a new scan of
/// the union is not built) or a key is not a column of those rows.
fn finest_common_group(
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
    let parameters: Vec<_> = match &expression {
        Operator::ASAP(ASAPOp::SummaryAgg {
            summary_type: FieldDataType::Sketch(algorithm),
            ..
        }) => parameter_list(*algorithm)
            .iter()
            .cloned()
            .map(Some)
            .collect(),
        _ => vec![None],
    };
    for theta in parameters {
        add_candidate(
            memo,
            finest,
            expression.clone(),
            theta,
            OriginOfCandidateForMemoGroup::Rollup,
        )?;
    }
    Ok(Some(finest))
}

/// Adds to `coarse` the candidate computing it from `fine` when (concepts
/// doc, section 3, conditions 1–5) its keys are among `fine`'s, its
/// selection lies inside `fine`'s, narrower only on `fine`'s keys, and the
/// cluster's equal definition and time window hold:
/// `Rollup(by coarse's keys)(Filter(compensation, fine))`, without the
/// `Filter` when the selections are equal. States roll up with a
/// regrouping `SummaryMerge`; sums and counts with a sum, minima and maxima
/// with themselves.
fn rollup_from(memo: &mut Memo, coarse: &Member, fine: &Member) -> Result<(), Pass2RuleError> {
    if !coarse.keys.iter().all(|key| fine.keys.contains(key)) {
        return Ok(());
    }
    let mut compensation = Vec::new();
    for column in coarse.selection.keys().chain(fine.selection.keys()) {
        match (coarse.selection.get(column), fine.selection.get(column)) {
            (Some(narrow), wide) if Some(narrow) != wide => {
                let within = match (narrow, wide) {
                    (_, None) => true,
                    (Constraint::In(values), Some(Constraint::In(wider))) => {
                        values.iter().all(|value| wider.contains(value))
                    }
                    _ => false,
                };
                if !within || !fine.keys.contains(column) {
                    return Ok(());
                }
                if !compensation.iter().any(|(known, _)| known == column) {
                    compensation.push((column.clone(), narrow.clone()));
                }
            }
            (None, Some(_)) => return Ok(()),
            _ => {}
        }
    }
    if compensation.is_empty() && coarse.keys.len() == fine.keys.len() {
        return Ok(());
    }
    let fine_properties = &memo.groups()[fine.group.0 as usize].group_properties;
    let lineage = fine_properties.column_lineage.clone();
    let semantics = if fine_properties.result_kind == OperatorResultKind::Relation {
        ExprSemantics::Sql
    } else {
        ExprSemantics::Promql
    };
    let mut conjuncts = Vec::new();
    for (column, constraint) in &compensation {
        let Some(at) = column_of(&lineage, column) else {
            return Ok(());
        };
        let Some(conjunct) = selecting(at, constraint, semantics) else {
            return Ok(());
        };
        conjuncts.push(conjunct);
    }
    let input = match conjuncts.len() {
        0 => fine.group,
        _ => add_group_computing_itself(
            memo,
            Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(if conjuncts.len() == 1 {
                    conjuncts.remove(0)
                } else {
                    ScalarExpr::BoolAnd(conjuncts)
                }),
                child: fine.group,
            }),
            OriginOfCandidateForMemoGroup::Rollup,
        )?,
    };
    let Some(group_by) = coarse
        .keys
        .iter()
        .map(|key| column_of(&lineage, key))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(());
    };
    let rollup = match &coarse.expression {
        Operator::ASAP(ASAPOp::SummaryAgg { .. }) => Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![input],
            group_by: Some(group_by),
        }),
        Operator::NonASAP(NonASAPOp::Aggregate {
            measures,
            output_names,
            filters,
            ..
        }) => {
            let Some(measures) = measures
                .iter()
                .enumerate()
                .map(|(index, measure)| rolled_up(measure, fine.keys.len() + index))
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(());
            };
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(group_by),
                measures,
                output_names: output_names.clone(),
                filters: vec![None; filters.len()],
                having: None,
                child: input,
            })
        }
        _ => unreachable!("a member is a SummaryAgg or an Aggregate"),
    };
    add_candidates_per_input_parameters(
        memo,
        coarse.group,
        rollup,
        OriginOfCandidateForMemoGroup::Rollup,
    )
}

/// The measure rolling `measure`'s finalized values up from column
/// `column` of the finer result: a sum or count by a sum, a minimum or
/// maximum by itself; `None` for any other measure (it rolls up only from
/// states).
fn rolled_up(measure: &AggIntent, column: usize) -> Option<AggIntent> {
    let col = Some(column);
    Some(match measure {
        AggIntent::Sum { .. } | AggIntent::Count => AggIntent::Sum { col },
        AggIntent::Min { .. } => AggIntent::Min { col },
        AggIntent::Max { .. } => AggIntent::Max { col },
        _ => return None,
    })
}

/// The column of `lineage` that copies `column`, when exactly one does.
fn column_of(lineage: &[ColumnLineage], column: &SourceColumn) -> Option<usize> {
    let mut at = lineage
        .iter()
        .enumerate()
        .filter(|(_, known)| matches!(known, ColumnLineage::SourceColumn(c) if c == column))
        .map(|(index, _)| index);
    let first = at.next()?;
    at.next().is_none().then_some(first)
}

/// The predicate selecting the rows whose column `at` meets `constraint`:
/// `at = v` or `at IN (…)`, `at NOT IN (…)`; `None` for an interval (never
/// a compensation: the time window is equal across a cluster).
fn selecting(
    at: usize,
    constraint: &Constraint,
    semantics: ExprSemantics,
) -> Option<ScalarExpr<MemoGroupId>> {
    let literal = |value: &ScalarValue| ScalarExpr::Literal(value.clone());
    Some(match constraint {
        Constraint::In(values) if values.len() == 1 => ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(at)),
            op: CompareOpKind::Eq,
            right: Box::new(literal(&values[0])),
            semantics,
        },
        Constraint::In(values) | Constraint::NotIn(values) => ScalarExpr::InList {
            expr: Box::new(ScalarExpr::Column(at)),
            list: values.iter().map(literal).collect(),
            negated: matches!(constraint, Constraint::NotIn(_)),
        },
        Constraint::Interval { .. } => return None,
    })
}
