//! IR-building helpers both Stage 1 passes use. Neither pass imports the
//! other (`tests/pass_layering.rs`), so what they share lives here.

use asap_types::ir::operator::Reduction;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{ExactKind, ExactParams, Field, FieldDataType, Schema};

/// A reference to `column`: qualified by its table when it has one.
pub(crate) fn column_ref(column: &Field) -> ColumnRef {
    match &column.table {
        Some(t) => ColumnRef::Qualified {
            table: t.clone(),
            name: column.name.clone(),
        },
        None => ColumnRef::Named(column.name.clone()),
    }
}

/// `FieldDataType::ExactAggregate` of accumulator `kind`, with its parameters
/// (none has any, so they mirror `kind`).
pub(crate) fn exact_family(kind: ExactKind) -> FieldDataType {
    let params = match kind {
        ExactKind::Sum => ExactParams::Sum,
        ExactKind::Count => ExactParams::Count,
        ExactKind::Min => ExactParams::Min,
        ExactKind::Max => ExactParams::Max,
        ExactKind::Rate => ExactParams::Rate,
        ExactKind::IRate => ExactParams::IRate,
        ExactKind::Increase => ExactParams::Increase,
    };
    FieldDataType::ExactAggregate(kind, params)
}

/// `reduction`'s keys, which index `inner` (the absorbed aggregate's
/// output), as indices into `input` (the rows the heap reads), matched by
/// column. `None` when a key is missing or ambiguous in `input`, or the keys
/// are given by `without`: `input` carries columns `inner` dropped.
pub(crate) fn partitions_over(
    reduction: &Reduction,
    inner: &Schema,
    input: &Schema,
) -> Option<Reduction> {
    let Reduction::Reduce(keys) = reduction else {
        return Some(Reduction::PerEntity);
    };
    if keys.is_without() {
        return None;
    }
    let column = |schema: &Schema, index: usize| column_ref(&schema.fields[index]);
    let keys = keys
        .keys()
        .iter()
        .map(|&index| {
            inner.fields.get(index)?;
            let key = column(inner, index);
            let mut matches = (0..input.fields.len()).filter(|&i| column(input, i) == key);
            match (matches.next(), matches.next()) {
                (Some(i), None) => Some(i),
                _ => None,
            }
        })
        .collect::<Option<_>>()?;
    Some(Reduction::by(keys))
}
