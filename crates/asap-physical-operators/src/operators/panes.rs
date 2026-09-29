//! Run-scoped pane population checks and timestamp restoration after reduction.
use super::*;
use crate::runtime::Scope;
use planner_types::post_asap::{validate_pane_coverage, PaneLayout, WindowEdgeCoverage};

impl Operator {
    pub(crate) fn pane_input(
        input: Schema,
        coordinate: usize,
        layout: PaneLayout,
        offset_ms: Option<i64>,
    ) -> Result<Self, Error> {
        if plain(&input, coordinate)? != (&DataType::Timestamp, false) {
            return Err(invalid("pane input requires a non-null timestamp"));
        }
        if layout.pane_width_ms > i64::MAX as u64 {
            return Err(invalid("pane width exceeds timestamp range"));
        }
        validate_pane_coverage(
            &layout,
            layout.pane_origin_ms,
            &WindowEdgeCoverage::PaneAligned,
        )
        .map_err(|error| Error::Invalid(format!("invalid pane layout: {error:?}")))?;
        if offset_ms.is_some_and(|offset| offset < 0) {
            return Err(invalid("negative pane offset"));
        }
        Ok(Self {
            kind: Kind::PaneInput {
                coordinate,
                layout,
                offset_ms,
            },
            inputs: vec![input.clone()],
            output: input,
        })
    }

    pub(crate) fn scope_timestamp(input: Schema, output: Schema) -> Result<Self, Error> {
        crate::values::validate_schema(&output)?;
        let coordinate = output
            .time_index
            .ok_or_else(|| invalid("temporal output requires a time index"))?;
        if plain(&output, coordinate)? != (&DataType::Timestamp, false) {
            return Err(invalid("temporal output requires a non-null timestamp"));
        }
        let mut columns = Vec::new();
        let mut used = std::collections::BTreeSet::new();
        for (index, field) in output.fields.iter().enumerate() {
            if index == coordinate {
                columns.push(None);
                continue;
            }
            let matches: Vec<_> = input
                .fields
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate.dtype == field.dtype
                        && candidate.nullable == field.nullable
                        && (candidate.name == field.name
                            || !matches!(field.dtype, SummaryFamilyType::Plain(_)))
                })
                .map(|(index, _)| index)
                .collect();
            let [column] = matches.as_slice() else {
                return Err(invalid("temporal output column missing or ambiguous"));
            };
            if !used.insert(*column) {
                return Err(invalid("temporal output repeats an input column"));
            }
            columns.push(Some(*column));
        }
        if used.len() != input.fields.len() {
            return Err(invalid("temporal output drops an input column"));
        }
        Ok(Self {
            kind: Kind::ScopeTimestamp { columns },
            inputs: vec![input],
            output,
        })
    }
}

pub(super) fn validate_context(operator: &Operator, context: &RunContext) -> Result<(), Error> {
    let Kind::PaneInput {
        layout, offset_ms, ..
    } = &operator.kind
    else {
        return Ok(());
    };
    let end = match (&context.scope, offset_ms) {
        (
            Scope::Ingestion {
                window_start_ms,
                window_end_ms,
                ..
            },
            None,
        ) => {
            if window_end_ms.checked_sub(*window_start_ms) != Some(layout.pane_width_ms as i64) {
                return Err(invalid("maintenance run must cover exactly one pane"));
            }
            *window_end_ms
        }
        (
            Scope::Query {
                evaluation_time_ms, ..
            },
            Some(offset),
        ) => {
            let end = evaluation_time_ms
                .checked_sub(*offset)
                .ok_or_else(|| invalid("query pane timestamp overflows"))?;
            end.checked_sub(layout.pane_width_ms as i64)
                .ok_or_else(|| invalid("query pane start overflows"))?;
            end
        }
        _ => return Err(invalid("pane operator received the wrong execution scope")),
    };
    validate_pane_coverage(layout, Some(end), &WindowEdgeCoverage::PaneAligned).map_err(|error| {
        Error::Invalid(format!(
            "query requires aligned panes or boundary residuals: {error:?}"
        ))
    })
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    validate_context(operator, &context)?;
    let input = inputs.pop().ok_or_else(|| invalid("pane input missing"))?;
    let output = operator.output.clone();
    let mut seen = std::collections::BTreeSet::new();
    let mut memory = context.reserve(0)?;
    let mut key_bytes = 0;
    Ok(input
        .map(move |batch| {
            if context.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let batch = batch?;
            match &operator.kind {
                Kind::PaneInput {
                    coordinate,
                    offset_ms,
                    ..
                } => {
                    let groups: Vec<_> = output
                        .fields
                        .iter()
                        .enumerate()
                        .filter(|(index, field)| {
                            *index != *coordinate
                                && matches!(field.dtype, SummaryFamilyType::Plain(_))
                        })
                        .map(|(index, _)| index)
                        .collect();
                    for row in batch.rows() {
                        let Value::Timestamp(timestamp) = row[*coordinate] else {
                            return Err(invalid("pane timestamp type mismatch"));
                        };
                        match (&context.scope, offset_ms) {
                            (
                                Scope::Ingestion {
                                    window_start_ms,
                                    window_end_ms,
                                    ..
                                },
                                None,
                            ) if timestamp > *window_start_ms && timestamp <= *window_end_ms => {}
                            (
                                Scope::Query {
                                    evaluation_time_ms, ..
                                },
                                Some(offset),
                            ) if timestamp
                                == evaluation_time_ms
                                    .checked_sub(*offset)
                                    .ok_or_else(|| invalid("pane timestamp overflows"))? =>
                            {
                                let key = group_key(row, &groups)?;
                                if seen.contains(&key) {
                                    return Err(invalid("duplicate entity state within a pane"));
                                }
                                key_bytes += key.iter().map(Vec::len).sum::<usize>()
                                    + key.len() * std::mem::size_of::<Vec<u8>>()
                                    + 64;
                                memory.resize(key_bytes)?;
                                seen.insert(key);
                            }
                            _ => {
                                return Err(invalid("input population differs from required pane"))
                            }
                        }
                    }
                    Ok(batch.value().clone())
                }
                Kind::ScopeTimestamp { columns } => {
                    let timestamp = match context.scope {
                        Scope::Ingestion { window_end_ms, .. } => window_end_ms,
                        Scope::Query {
                            evaluation_time_ms, ..
                        } => evaluation_time_ms,
                    };
                    let rows = batch
                        .rows()
                        .iter()
                        .map(|row| {
                            columns
                                .iter()
                                .map(|column| {
                                    column.map_or(Value::Timestamp(timestamp), |column| {
                                        row[column].clone()
                                    })
                                })
                                .collect()
                        })
                        .collect();
                    Batch::try_new(output.clone(), rows)
                }
                _ => unreachable!(),
            }
        })
        .boxed_local())
}
