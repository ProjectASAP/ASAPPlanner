//! Run-scoped timestamp restoration after reduction.
use super::*;
use crate::runtime::Scope;

impl Operator {
    /// `input`'s columns as `output`'s, its time column stamped with the
    /// run's time: an ingestion run's window end, a query run's evaluation
    /// time.
    pub(crate) fn scope_timestamp(input: SchemaRef, output: SchemaRef) -> Result<Self, Error> {
        Self::window_end_timestamp(input, output, None)
    }

    /// As [`Self::scope_timestamp`], except that with `window_end`, the
    /// time column copies that input column: each row's window end, for the
    /// states of a tumbling-window chain built at query time, one per
    /// window ([`Self::with_tumbling_window_ends`]).
    pub(crate) fn window_end_timestamp(
        input: SchemaRef,
        output: SchemaRef,
        window_end: Option<usize>,
    ) -> Result<Self, Error> {
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
                if let Some(window_end) = window_end {
                    if plain(&input, window_end)?.0 != &DataType::Timestamp
                        || !used.insert(window_end)
                    {
                        return Err(invalid("window end column must be a timestamp"));
                    }
                }
                columns.push(window_end);
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

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::ScopeTimestamp { columns } = &operator.kind else {
        return Err(invalid("scope timestamp operator required"));
    };
    let input = inputs
        .pop()
        .ok_or_else(|| invalid("scope timestamp input missing"))?;
    let output = operator.output.clone();
    let timestamp = match context.scope {
        Scope::Ingestion { window_end_ms, .. } => window_end_ms,
        Scope::Query {
            evaluation_time_ms, ..
        } => evaluation_time_ms,
    };
    // Columns are shared, not copied; only the timestamp column is new.
    Ok(input
        .map(move |batch| {
            if context.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let batch = batch?;
            let rows = batch.num_rows();
            let mut arrays = Vec::with_capacity(columns.len());
            let mut states = Vec::with_capacity(columns.len());
            for column in columns {
                match column {
                    Some(column) => {
                        arrays.push(batch.column(*column).clone());
                        states.push(batch.state_column(*column).cloned());
                    }
                    None => {
                        arrays.push(std::sync::Arc::new(
                            arrow::array::TimestampMillisecondArray::from(vec![timestamp; rows]),
                        ) as arrow::array::ArrayRef);
                        states.push(None);
                    }
                }
            }
            Batch::from_parts(output.clone(), arrays, states, rows)
        })
        .boxed_local())
}
