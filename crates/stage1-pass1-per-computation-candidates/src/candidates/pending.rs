//! A way to compute a group, built pending before it is added: its
//! expressions are checked against the schemas the memo derives for them
//! (`Memo::output_schema_of_pending_expression`) and added only once every
//! check that can find the way unsupported has passed, so an unsupported
//! way adds nothing.

use asap_types::ir::schema::FieldDataType;
use asap_types::ir::schema::Schema;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{
    CandidateExpression, CandidateForMemoGroup, LogicalProperties, LogicalRequirements, Memo,
    MemoGroupId, OriginOfCandidateForMemoGroup,
};
use asap_types::summary_type::{parameter_list, SketchParams};

use crate::{schema, PerComputationCandidateError};

/// The expressions of one way to compute a group, not yet in the memo, in
/// the order they are added, each with its derived output schema. Each
/// reads memo groups or earlier pending expressions: pending expression `k`
/// is named `MemoGroupId(first_id + k)`, the id it would get as a new group.
pub(crate) struct Pending {
    /// The memo's group count when the way was started.
    first_id: usize,
    /// Each pending expression with its output schema, derived once when it
    /// was pushed.
    expressions_with_schemas: Vec<(Operator<MemoGroupId>, Schema)>,
}

impl Pending {
    /// An empty way over `memo`'s groups.
    pub(crate) fn new(memo: &Memo) -> Self {
        Self {
            first_id: memo.groups().len(),
            expressions_with_schemas: Vec::new(),
        }
    }

    /// Appends `expression`, once its output schema derives over `memo` and
    /// the earlier pending expressions, and returns the id that names it.
    /// An expression that does not derive is a bug in the rule that built it.
    pub(crate) fn push(
        &mut self,
        memo: &Memo,
        expression: Operator<MemoGroupId>,
    ) -> Result<MemoGroupId, PerComputationCandidateError> {
        let schema =
            memo.output_schema_of_pending_expression(&self.expressions_with_schemas, &expression)?;
        let id = MemoGroupId(
            u32::try_from(self.first_id + self.expressions_with_schemas.len())
                .expect("a memo has at most u32::MAX groups"),
        );
        self.expressions_with_schemas.push((expression, schema));
        Ok(id)
    }

    /// The last pushed expression, removed: it becomes a candidate rather
    /// than a group.
    pub(crate) fn pop(&mut self) -> Operator<MemoGroupId> {
        self.expressions_with_schemas
            .pop()
            .expect("an expression was pushed")
            .0
    }

    /// The output schema of `group`: a memo group's, or a pending
    /// expression's.
    pub(crate) fn schema<'a>(&'a self, memo: &'a Memo, group: MemoGroupId) -> &'a Schema {
        match (group.0 as usize).checked_sub(self.first_id) {
            None => schema(memo, group),
            Some(index) => &self.expressions_with_schemas[index].1,
        }
    }
}

/// Adds `pending` to `memo` in order, each pending input replaced by the
/// group it became: a summary build as a state group with its build
/// candidates, listing `fused_groups` ([`add_state_group`]), any other
/// expression as its own group's candidate ([`add_group`]). Returns what
/// each id names now: a memo group stays, a pending id becomes the group its
/// expression became. The caller has already checked the way, so a failure
/// here is a bug.
///
/// # Panics
///
/// When `memo` gained groups since the way was started.
pub(crate) fn commit_pending(
    memo: &mut Memo,
    pending: Pending,
    fused_groups: &[MemoGroupId],
) -> Result<impl Fn(MemoGroupId) -> MemoGroupId, PerComputationCandidateError> {
    let first_id = pending.first_id;
    assert_eq!(
        memo.groups().len(),
        first_id,
        "no group is added between starting and adding a way"
    );
    let mut groups: Vec<MemoGroupId> = Vec::with_capacity(pending.expressions_with_schemas.len());
    let group_of = move |groups: &[MemoGroupId], id: MemoGroupId| {
        (id.0 as usize)
            .checked_sub(first_id)
            .map_or(id, |index| groups[index])
    };
    for (expression, _) in pending.expressions_with_schemas {
        let expression = expression.map_children(|&input| group_of(&groups, input));
        let group = match expression {
            Operator::ASAP(ASAPOp::SummaryAgg { .. }) => {
                add_state_group(memo, expression, fused_groups)?
            }
            _ => add_group(memo, expression)?,
        };
        groups.push(group);
    }
    Ok(move |id| group_of(&groups, id))
}

/// The group of `operator`, whatever adds it, with `operator` as its
/// candidate: an intermediate result is its own group.
pub(crate) fn add_group(
    memo: &mut Memo,
    operator: Operator<MemoGroupId>,
) -> Result<MemoGroupId, PerComputationCandidateError> {
    let group = memo.find_or_add_group(operator.clone())?;
    add_candidate(memo, group, operator, None, Vec::new())?;
    Ok(group)
}

/// The state group of `build` (a `SummaryAgg`) with its build candidates:
/// one per configuration of a sketch algorithm's predefined list
/// (`parameter_list`), or the one exact accumulator build. Each lists
/// `fused_groups`.
pub(crate) fn add_state_group(
    memo: &mut Memo,
    build: Operator<MemoGroupId>,
    fused_groups: &[MemoGroupId],
) -> Result<MemoGroupId, PerComputationCandidateError> {
    let group = memo.find_or_add_group(build.clone())?;
    let configurations: Vec<Option<SketchParams>> = match &build {
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
    for parameters in configurations {
        add_candidate(
            memo,
            group,
            build.clone(),
            parameters,
            fused_groups.to_vec(),
        )?;
    }
    Ok(group)
}

/// Adds the candidate `operator` with `parameters` to `group`. Its input
/// requirements are empty: Pass 2 sets each readout's required accuracy
/// (`Memo::set_candidate_input_requirements`).
pub(crate) fn add_candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
    fused_groups: Vec<MemoGroupId>,
) -> Result<(), PerComputationCandidateError> {
    let expression = CandidateExpression { operator };
    let logical_properties = LogicalProperties::new(&expression, parameters)?;
    let input_requirements =
        vec![LogicalRequirements::default(); expression.operator.children().len()];
    memo.add_candidate(
        group,
        CandidateForMemoGroup {
            expression,
            logical_properties,
            input_requirements,
            fused_groups,
            origin: OriginOfCandidateForMemoGroup::Pass1,
        },
    )?;
    Ok(())
}
