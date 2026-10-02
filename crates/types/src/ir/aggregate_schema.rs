//! Derive aggregate output columns and types from input schema and reduction.
//!
//! [`aggregate_output_schema`] handles grouping keys and aggregate results.
//! For example, SQL `GROUP BY host` retains the grouping column and adds the
//! aggregate result; a PromQL per-series range reduction preserves labels
//! and produces a Float64 sample value. This module derives schemas, not
//! aggregate values or summary candidates.
use super::operator_properties::*;
use super::QueryExprError;
use crate::pre_asap::{AggIntent, ColumnId, ColumnRef, DataType, Field, FieldDataType, Schema};
/// Output schema of a *per-series* window/range reduction (`rate`/`increase`,
/// or an `*_over_time` reducer under a time `Window`). Such a reduction emits
/// one value per series, so every label column of `input` is preserved and only
/// the sample value is replaced — kept named `value` so the PromQL sample-value
/// convention (and any outer `SampleValue` reference) still resolves it by name.
fn per_series_reduction_schema(input: &Schema, agg: &AggIntent) -> Result<Schema, QueryExprError> {
    let vi = if let Some(index) = agg.input_cols().first() {
        *index
    } else {
        crate::pre_asap::column_resolution::resolve_column_ref(&ColumnRef::SampleValue, input)
            .map_err(|error| QueryExprError::InvalidSampleColumn(error.to_string()))?
    };
    if !matches!(
        input.fields.get(vi).map(|column| &column.dtype),
        Some(FieldDataType::Plain(DataType::Float64 | DataType::Int64))
    ) {
        return Err(QueryExprError::InvalidSampleColumn(format!(
            "column {vi} is not numeric"
        )));
    }
    let mut columns = input.fields.clone();
    {
        let mut out = agg.output_column(&columns[vi]);
        out.name = "value".into();
        // A per-series range reduction produces a PromQL sample value, which is
        // always `float64` — override the reducer's own output dtype so
        // `count_over_time` (whose `Count` intent types `Int64`) matches every
        // other range reducer instead of leaking an `Int64` value column (#69).
        out.dtype = FieldDataType::Plain(DataType::Float64);
        columns[vi] = out;
    }
    Ok(Schema {
        fields: columns,
        time_index: input.time_index,
        unique_keys: input.unique_keys.clone(),
        // Per-series reduction is label-preserving: it inherits its input's
        // completeness (an open scan stays open; a closed one stays closed).
        closed: input.closed,
    })
}

/// The output schema of an `Aggregate { reduction, measures }` over `in_schema` —
/// the **single** canonical derivation shared by
/// [`NonASAPOp::output_schema`](crate::ir::NonASAPOp::output_schema)'s
/// `Aggregate` arm and the HAVING-resolution path (`column_resolution::output_schema_for_aggregate`),
/// so the two can never drift (issue #41).
///
/// `Reduction::PerEntity` selects the label-preserving
/// [`per_series_reduction_schema`] (`rate`/`increase`/`*_over_time`) instead
/// of the cross-series `by ++ measures` shape. Which one applies is read directly
/// off `reduction` — decided once, at construction, by whoever built the
/// `Aggregate` node (issue #165) — not re-derived here from `by`/child shape.
pub fn aggregate_output_schema(
    in_schema: &Schema,
    reduction: &Reduction,
    measures: &[AggIntent],
    output_names: &[String],
) -> Result<Schema, QueryExprError> {
    let by = match reduction {
        Reduction::PerEntity => {
            debug_assert_eq!(
                measures.len(),
                1,
                "a per-entity reduction is single-aggregate"
            );
            return per_series_reduction_schema(in_schema, &measures[0]);
        }
        Reduction::Reduce(by) => by,
    };

    // `without(excluded)` groups by every label *except* those listed: the kept
    // labels are the input's label columns minus the excluded positions (and the
    // ts / sample-value columns), and the schema stays **open** because the full
    // runtime label set isn't known. The `by(...)` path instead enumerates its
    // kept columns and freezes to closed (issue #39).
    if by.is_without() {
        return without_output_schema(in_schema, by.keys(), measures, output_names);
    }

    let mut out_cols: Vec<Field> = Vec::with_capacity(by.len() + measures.len());
    for &id in by.keys() {
        let c = in_schema
            .fields
            .get(id)
            .ok_or(QueryExprError::InvalidGroupByColumn(
                id,
                in_schema.fields.len(),
            ))?;
        out_cols.push(c.clone());
    }
    let value_col_idx =
        crate::pre_asap::column_resolution::resolve_column_ref(&ColumnRef::SampleValue, in_schema)
            .ok()
            .or_else(|| (0..in_schema.fields.len()).find(|i| !by.contains(i)));
    let probe = value_col_idx
        .and_then(|i| in_schema.fields.get(i))
        .cloned()
        .unwrap_or_else(|| Field::plain("value", DataType::Float64, false));
    // Each reducer types off its own input column (`SUM(bytes)` vs `AVG(latency)`
    // in one node); `None` falls back to the sample-value probe (PromQL's
    // single-column convention). A non-empty `output_names[i]` overrides the
    // synthetic output column name.
    for (i, intent) in measures.iter().enumerate() {
        // `count_values("l", v)` emits TWO columns: the synthesized `Utf8` label
        // `l` (the stringified sample value it groups by) and the per-value
        // count. If `l` collides with a group-by key of the same name, PromQL's
        // synthesized label takes precedence — emit a single column, never a
        // duplicate.
        if let AggIntent::CountValues { label } = intent {
            if !out_cols.iter().any(|c| c.name == *label) {
                out_cols.push(Field::plain(label.clone(), DataType::Utf8, false));
            }
            let mut cnt = intent.output_column(&probe);
            if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
                cnt.name = name.clone();
            }
            out_cols.push(cnt);
            continue;
        }
        // Only the output *type* is read from here, so the leading column is
        // enough for the multi-column intents: `Cardinality` and `PearsonCorr`
        // both have a fixed output type that ignores it.
        let in_col = intent
            .input_cols()
            .first()
            .and_then(|id| in_schema.fields.get(*id))
            .unwrap_or(&probe);
        let mut out = intent.output_column(in_col);
        // A global extremum emits NULL for an empty input, even if its input
        // column is non-nullable. Grouped extrema only emit existing groups.
        if by.is_empty() && matches!(intent, AggIntent::Min { .. } | AggIntent::Max { .. }) {
            out.nullable = true;
        }
        if let Some((arg, _)) = intent
            .arg_selector_columns(in_schema)
            .map_err(QueryExprError::InvalidScalarSignature)?
        {
            out.dtype = in_schema.fields[arg].dtype.clone();
            out.nullable = in_schema.fields[arg].nullable;
        }
        if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
            out.name = name.clone();
        }
        out_cols.push(out);
    }
    // `count_values` groups by (by-keys ∪ the synthesized value label), so the
    // by-keys alone are not a unique key — be conservative and claim none.
    let has_count_values = measures
        .iter()
        .any(|a| matches!(a, AggIntent::CountValues { .. }));
    let unique_keys = if by.is_empty() || has_count_values {
        Vec::new()
    } else {
        vec![(0..by.len()).collect()]
    };
    Ok(Schema {
        fields: out_cols,
        time_index: None,
        unique_keys,
        // A cross-series aggregate enumerates exactly `by ++ measures`, so its output
        // is closed even over an open input — this is where an open schema
        // freezes to closed.
        closed: true,
    })
}

/// Output schema of a `without(excluded)` aggregate: the kept labels (every
/// input label column except the `excluded` positions, the time axis, and the
/// sample-value column) followed by the aggregate output column(s). Unlike the
/// `by` path this stays **open** — the excluded set is enumerable but the kept
/// set is not (the runtime carries labels the usage-derived schema never saw),
/// so the schema can't freeze to closed and claims no unique key (issue #39).
fn without_output_schema(
    in_schema: &Schema,
    excluded: &[ColumnId],
    measures: &[AggIntent],
    output_names: &[String],
) -> Result<Schema, QueryExprError> {
    for &id in excluded {
        if id >= in_schema.fields.len() {
            return Err(QueryExprError::InvalidGroupByColumn(
                id,
                in_schema.fields.len(),
            ));
        }
    }
    let mut out_cols: Vec<Field> = Vec::new();
    for (i, col) in in_schema.fields.iter().enumerate() {
        let is_time = in_schema.time_index == Some(i);
        let is_value = crate::pre_asap::column_resolution::resolve_column_ref(
            &ColumnRef::SampleValue,
            in_schema,
        )
        .ok()
            == Some(i);
        if !is_time && !is_value && !excluded.contains(&i) {
            out_cols.push(col.clone());
        }
    }
    let probe = in_schema
        .column_id("value")
        .and_then(|i| in_schema.fields.get(i))
        .cloned()
        .unwrap_or_else(|| Field::plain("value", DataType::Float64, false));
    for (i, intent) in measures.iter().enumerate() {
        // Only the output *type* is read from here, so the leading column is
        // enough for the multi-column intents: `Cardinality` and `PearsonCorr`
        // both have a fixed output type that ignores it.
        let in_col = intent
            .input_cols()
            .first()
            .and_then(|id| in_schema.fields.get(*id))
            .unwrap_or(&probe);
        let mut out = intent.output_column(in_col);
        if let Some((arg, _)) = intent
            .arg_selector_columns(in_schema)
            .map_err(QueryExprError::InvalidScalarSignature)?
        {
            out.dtype = in_schema.fields[arg].dtype.clone();
            out.nullable = in_schema.fields[arg].nullable;
        }
        if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
            out.name = name.clone();
        }
        out_cols.push(out);
    }
    Ok(Schema {
        fields: out_cols,
        time_index: None,
        unique_keys: Vec::new(),
        // The kept label set is runtime-only, so — unlike `by` — this does not
        // freeze the open schema to closed.
        closed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, dtype: DataType, nullable: bool) -> Field {
        Field::plain(name, dtype, nullable)
    }

    #[test]
    fn group_keys_by_vs_without_semantics() {
        let by = GroupKeys::by(vec![1, 2]);
        let without = GroupKeys::without(vec![1, 2]);
        assert!(!by.is_without());
        assert!(without.is_without());
        // Deref / iteration expose the stored keys regardless of mode.
        assert_eq!(by.len(), 2);
        assert_eq!(without.keys(), &[1, 2]);
        // A `by` compares equal to its bare vec; a `without` never does.
        assert_eq!(by, vec![1, 2]);
        assert_ne!(without, vec![1, 2]);
        assert_ne!(by, without);
    }

    #[test]
    fn group_keys_serde_by_is_bare_array_without_is_tagged() {
        // `by` keeps the pre-#39 bare-array wire format; `without` uses an object.
        let by = serde_json::to_string(&GroupKeys::by(vec![2, 3])).unwrap();
        assert_eq!(by, "[2,3]");
        let without = serde_json::to_string(&GroupKeys::without(vec![2])).unwrap();
        assert_eq!(without, r#"{"without":[2]}"#);
        // Round-trip both.
        for g in [GroupKeys::by(vec![2, 3]), GroupKeys::without(vec![2])] {
            let json = serde_json::to_string(&g).unwrap();
            let back: GroupKeys = serde_json::from_str(&json).unwrap();
            assert_eq!(back, g);
        }
    }

    #[test]
    fn time_shift_identity_and_serde() {
        let offset_only = TimeShift {
            offset_ms: 1,
            at: None,
        };
        let at_only = TimeShift {
            offset_ms: 0,
            at: Some(AtModifier::End),
        };
        assert!(TimeShift::default().is_identity());
        assert!(!offset_only.is_identity());
        assert!(!at_only.is_identity());
        // Round-trip the shift + anchor.
        let s = TimeShift {
            offset_ms: -300_000,
            at: Some(AtModifier::Timestamp(60_000)),
        };
        let back: TimeShift = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    // Nested temporal aggregation must replace the sample, never the grouping label.
    #[test]
    fn temporal_reduction_of_grouped_sum_preserves_job() {
        let input = Schema::new(vec![
            col("job", DataType::Utf8, true),
            col("sum", DataType::Float64, false),
        ]);
        for aggregate in [
            AggIntent::Avg { col: None },
            AggIntent::Avg { col: Some(1) },
            AggIntent::Rate,
        ] {
            let output =
                aggregate_output_schema(&input, &Reduction::PerEntity, &[aggregate], &[]).unwrap();
            assert_eq!(output.fields[0], input.fields[0]);
            assert_eq!(output.fields[1].name, "value");
            assert_eq!(output.fields[1].dtype, DataType::Float64);
        }
    }

    #[test]
    fn discriminator_assertion_rejects_unknown_wire_fields() {
        let json = r#"{"discriminator":1,"inner_key":[0],"unverified":true}"#;
        assert!(serde_json::from_str::<ConcatDiscriminatorKey>(json).is_err());
    }

    #[test]
    fn aggregate_strips_time_and_keeps_unique_keys() {
        let input = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("host", DataType::Utf8, false),
            ],
            0,
            Vec::new(),
        );
        let out = aggregate_output_schema(
            &input,
            &Reduction::by(vec![2]),
            &[AggIntent::Sum { col: None }],
            &[],
        )
        .expect("valid group-by column");
        let names: Vec<_> = out.fields.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["host", "sum"]);
        assert!(out.time_index.is_none());
        assert_eq!(out.unique_keys, vec![vec![0]]);
    }
}
