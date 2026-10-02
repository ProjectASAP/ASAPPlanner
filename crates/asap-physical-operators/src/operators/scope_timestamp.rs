//! Run-scoped timestamp restoration after reduction.
use super::*;
use crate::runtime::Scope;

impl Operator {
    pub(crate) fn scope_timestamp(input: SchemaRef, output: SchemaRef) -> Result<Self, Error> {
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
                            || !matches!(field.dtype, FieldDataType::Plain(_)))
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
    Ok(input
        .map(move |batch| {
            if context.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let batch = batch?;
            let rows = batch
                .rows()
                .iter()
                .map(|row| {
                    columns
                        .iter()
                        .map(|column| {
                            column.map_or(Value::Timestamp(timestamp), |column| row[column].clone())
                        })
                        .collect()
                })
                .collect();
            Batch::try_new(output.clone(), rows)
        })
        .boxed_local())
}
