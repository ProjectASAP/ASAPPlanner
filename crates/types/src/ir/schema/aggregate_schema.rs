//! Derive aggregate output columns and types from input schema and reduction.
//!
//! [`aggregate_output_schema`] handles grouping keys and aggregate results.
//! For example, SQL `GROUP BY host` retains the grouping column and adds the
//! aggregate result; a PromQL per-series range reduction preserves labels
//! and produces a Float64 sample value. This module derives schemas, not
//! aggregate values or summary candidates.
use crate::ir::operator::arguments::*;
use crate::ir::operator::AggIntent;
use crate::ir::scalar::ColumnRef;
use crate::ir::schema::{ColumnId, DataType, Field, FieldDataType, Schema};
use crate::ir::SchemaDerivationError;

/// Where one output column of an aggregate comes from: an input column it
/// keeps, a measure's value (with the input column the schema types it by,
/// the measure's own or the probe value column), or a label column the
/// aggregate computes (`count_values`' and `absent`'s; a series identity
/// a `without` grouping keeps, its labels less the excluded ones and
/// `__name__`; and a `by` or kept column that a `count_values` label of the
/// same name replaces: `count_values by (job) ("job", v)` →
/// `[ComputedLabel, Measure { .. }]`). `sum by (job) (x)` →
/// `[InputColumn(job), Measure { index: 0, input_column: Some(value) }]`. The one layout the output schema
/// ([`aggregate_output_schema`]) and the group properties read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputColumnSource {
    InputColumn(ColumnId),
    Measure {
        index: usize,
        input_column: Option<ColumnId>,
    },
    ComputedLabel,
}
/// Output schema of a *per-series* window/range reduction (`rate`/`increase`,
/// or an `*_over_time` reducer under a time `Window`). Such a reduction emits
/// one value per series, so every label column of `input` is preserved and only
/// the sample value is replaced — kept named `value` so the PromQL sample-value
/// convention (and any outer `SampleValue` reference) still resolves it by name.
fn per_series_reduction_schema(
    input: &Schema,
    agg: &AggIntent,
) -> Result<(Schema, Vec<OutputColumnSource>), SchemaDerivationError> {
    let vi = if let Some(index) = agg.input_cols().first() {
        *index
    } else {
        crate::ir::scalar::column_resolution::resolve_column_ref(&ColumnRef::SampleValue, input)
            .map_err(|error| SchemaDerivationError::InvalidSampleColumn(error.to_string()))?
    };
    if !matches!(
        input.fields.get(vi).map(|column| &column.dtype),
        Some(FieldDataType::Plain(DataType::Float64 | DataType::Int64))
    ) {
        return Err(SchemaDerivationError::InvalidSampleColumn(format!(
            "column {vi} is not numeric"
        )));
    }
    let mut columns = input.fields.clone();
    let mut sources: Vec<OutputColumnSource> = (0..columns.len())
        .map(OutputColumnSource::InputColumn)
        .collect();
    sources[vi] = OutputColumnSource::Measure {
        index: 0,
        input_column: Some(vi),
    };
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
    Ok((
        Schema {
            fields: columns,
            time_index: input.time_index,
            unique_keys: input.unique_keys.clone(),
            // Per-series reduction is label-preserving: it inherits its input's
            // completeness (an open scan stays open; a closed one stays closed).
            closed: input.closed,
        },
        sources,
    ))
}

/// The output schema of an `Aggregate { reduction, measures }` over `in_schema` —
/// the **single** canonical derivation shared by
/// `NonASAPOp::output_schema`'s
/// `Aggregate` arm and the HAVING-resolution path (`asap_frontend_common::resolve`),
/// so the two can never drift (issue #41).
///
/// `Reduction::PerEntity` selects the label-preserving
/// `per_series_reduction_schema` (`rate`/`increase`/`*_over_time`) instead
/// of the cross-series `by ++ measures` shape. Which one applies is read directly
/// off `reduction` — decided once, at construction, by whoever built the
/// `Aggregate` node — not re-derived here from `by`/child shape.
pub fn aggregate_output_schema(
    in_schema: &Schema,
    reduction: &Reduction,
    measures: &[AggIntent],
    output_names: &[String],
) -> Result<Schema, SchemaDerivationError> {
    Ok(aggregate_output(in_schema, reduction, measures, output_names)?.0)
}

/// [`aggregate_output_schema`] and where each of its columns comes from
/// ([`OutputColumnSource`]), in one pass.
pub(crate) fn aggregate_output(
    in_schema: &Schema,
    reduction: &Reduction,
    measures: &[AggIntent],
    output_names: &[String],
) -> Result<(Schema, Vec<OutputColumnSource>), SchemaDerivationError> {
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
    let mut sources: Vec<OutputColumnSource> = Vec::with_capacity(by.len() + measures.len());
    for &id in by.keys() {
        let c = in_schema
            .fields
            .get(id)
            .ok_or(SchemaDerivationError::InvalidGroupByColumn(
                id,
                in_schema.fields.len(),
            ))?;
        out_cols.push(c.clone());
        sources.push(OutputColumnSource::InputColumn(id));
    }
    let value_col_idx = crate::ir::scalar::column_resolution::resolve_column_ref(
        &ColumnRef::SampleValue,
        in_schema,
    )
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
        // count.
        if let AggIntent::CountValues { label } = intent {
            push_count_values_label(&mut out_cols, &mut sources, label);
            let mut cnt = intent.output_column(&probe);
            if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
                cnt.name = name.clone();
            }
            out_cols.push(cnt);
            sources.push(OutputColumnSource::Measure {
                index: i,
                input_column: value_col_idx,
            });
            continue;
        }
        // `absent(v)` emits its synthesized labels as `Utf8` columns before
        // its value.
        if let AggIntent::Absent { labels } = intent {
            out_cols.extend(
                labels
                    .keys()
                    .map(|name| Field::plain(name.clone(), DataType::Utf8, false)),
            );
            sources.extend(labels.keys().map(|_| OutputColumnSource::ComputedLabel));
        }
        // Only the output *type* is read from here, so the leading column is
        // enough for the multi-column intents: `Cardinality` and `PearsonCorr`
        // both have a fixed output type that ignores it.
        let input_column = measure_input_column(in_schema, intent);
        let in_col = input_column.map_or(&probe, |id| &in_schema.fields[id]);
        let mut out = intent.output_column(in_col);
        // A global extremum emits NULL for an empty input, even if its input
        // column is non-nullable. Grouped extrema only emit existing groups.
        if by.is_empty() && matches!(intent, AggIntent::Min { .. } | AggIntent::Max { .. }) {
            out.nullable = true;
        }
        if let Some((arg, _)) = intent
            .arg_selector_columns(in_schema)
            .map_err(SchemaDerivationError::InvalidScalarSignature)?
        {
            out.dtype = in_schema.fields[arg].dtype.clone();
            out.nullable = in_schema.fields[arg].nullable;
        }
        if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
            out.name = name.clone();
        }
        out_cols.push(out);
        sources.push(OutputColumnSource::Measure {
            index: i,
            input_column: input_column.or(value_col_idx),
        });
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
    Ok((
        Schema {
            fields: out_cols,
            time_index: None,
            unique_keys,
            // A cross-series aggregate enumerates exactly `by ++ measures`, so its output
            // is closed even over an open input — this is where an open schema
            // freezes to closed.
            closed: true,
        },
        sources,
    ))
}

/// Add `count_values("l", v)`'s label column `l`, the value text it groups
/// by, to an aggregate's output columns so far. A kept or `by` column named
/// `l` is replaced by it, as Prometheus overwrites that label: `count_values
/// by (job) ("job", v)` outputs one `job` column, holding the value text, a
/// label the aggregate computes.
fn push_count_values_label(
    out_cols: &mut Vec<Field>,
    sources: &mut Vec<OutputColumnSource>,
    label: &str,
) {
    let field = Field::plain(label, DataType::Utf8, false);
    match out_cols.iter().position(|column| column.name == label) {
        Some(position) => {
            out_cols[position] = field;
            sources[position] = OutputColumnSource::ComputedLabel;
        }
        None => {
            out_cols.push(field);
            sources.push(OutputColumnSource::ComputedLabel);
        }
    }
}

/// The input column a measure's output is typed by when it names one: its
/// leading input column (the multi-column intents' output type ignores the
/// rest).
fn measure_input_column(in_schema: &Schema, intent: &AggIntent) -> Option<ColumnId> {
    intent
        .input_cols()
        .first()
        .copied()
        .filter(|&id| id < in_schema.fields.len())
}

/// The columns of `in_schema` a `without(excluded)` grouping keeps, in
/// order: every column but the time axis, the sample value and the
/// `excluded` positions. `[t, value, job, instance, $promql_series_identity]`
/// under `without (instance)` keeps `[job, $promql_series_identity]`; the
/// groups are the distinct values of those columns (the identity less the
/// excluded labels, see [`super::is_label_set_column`]).
pub fn without_kept_columns(in_schema: &Schema, excluded: &[ColumnId]) -> Vec<ColumnId> {
    let value = crate::ir::scalar::column_resolution::resolve_column_ref(
        &ColumnRef::SampleValue,
        in_schema,
    )
    .ok();
    (0..in_schema.fields.len())
        .filter(|&i| in_schema.time_index != Some(i) && value != Some(i) && !excluded.contains(&i))
        .collect()
}

/// Whether a `without(excluded)` grouping over `in_schema` removes the
/// label `label`: an excluded column's name, or `__name__`, which PromQL's
/// `without` always drops. `count_values("__name__", v) without (instance)`
/// sets `__name__` to the value, then removes it.
fn removed_by_without(in_schema: &Schema, excluded: &[ColumnId], label: &str) -> bool {
    label == "__name__"
        || excluded
            .iter()
            .any(|&id| in_schema.fields[id].name == label)
}

/// Output schema of a `without(excluded)` aggregate: the kept labels (every
/// input label column except the `excluded` positions, the time axis, and the
/// sample-value column) followed by the aggregate output column(s), a
/// `count_values` label before its count as on the `by` path.
///
/// A kept series identity or label map is not copied: its labels less the
/// excluded ones and `__name__` are computed, so its source is
/// [`OutputColumnSource::ComputedLabel`]. Under `without (instance)` the
/// identity `{__name__=m, job=a, instance=1, zone=z}` becomes
/// `{job=a, zone=z}`. A `count_values("zone", v)` label also replaces the
/// identity's `zone`, as Prometheus overwrites it before grouping, unless
/// the grouping removes `zone` ([`removed_by_without`]): then it is removed
/// with the excluded labels. Unlike the
/// `by` path this stays **open** — the excluded set is enumerable but the kept
/// set is not (the runtime carries labels the usage-derived schema never saw),
/// so the schema can't freeze to closed and claims no unique key (issue #39).
fn without_output_schema(
    in_schema: &Schema,
    excluded: &[ColumnId],
    measures: &[AggIntent],
    output_names: &[String],
) -> Result<(Schema, Vec<OutputColumnSource>), SchemaDerivationError> {
    for &id in excluded {
        if id >= in_schema.fields.len() {
            return Err(SchemaDerivationError::InvalidGroupByColumn(
                id,
                in_schema.fields.len(),
            ));
        }
    }
    let kept = without_kept_columns(in_schema, excluded);
    let mut out_cols: Vec<Field> = kept.iter().map(|&i| in_schema.fields[i].clone()).collect();
    let mut sources: Vec<OutputColumnSource> = kept
        .into_iter()
        .map(|i| {
            if super::is_label_set_column(&in_schema.fields[i]) {
                OutputColumnSource::ComputedLabel
            } else {
                OutputColumnSource::InputColumn(i)
            }
        })
        .collect();
    let probe = in_schema
        .column_id("value")
        .and_then(|i| in_schema.fields.get(i))
        .cloned()
        .unwrap_or_else(|| Field::plain("value", DataType::Float64, false));
    for (i, intent) in measures.iter().enumerate() {
        // A label the grouping removes is set and then removed, as in
        // Prometheus: `count_values("zone", v) without (instance, zone)`
        // counts per kept group, with no `zone` label. A `without` grouping
        // always removes `__name__` too.
        if let AggIntent::CountValues { label } = intent {
            if !removed_by_without(in_schema, excluded, label) {
                push_count_values_label(&mut out_cols, &mut sources, label);
            }
        }
        // Only the output *type* is read from here, so the leading column is
        // enough for the multi-column intents: `Cardinality` and `PearsonCorr`
        // both have a fixed output type that ignores it.
        let input_column = measure_input_column(in_schema, intent);
        let in_col = input_column.map_or(&probe, |id| &in_schema.fields[id]);
        let mut out = intent.output_column(in_col);
        if let Some((arg, _)) = intent
            .arg_selector_columns(in_schema)
            .map_err(SchemaDerivationError::InvalidScalarSignature)?
        {
            out.dtype = in_schema.fields[arg].dtype.clone();
            out.nullable = in_schema.fields[arg].nullable;
        }
        if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
            out.name = name.clone();
        }
        out_cols.push(out);
        sources.push(OutputColumnSource::Measure {
            index: i,
            input_column: input_column.or_else(|| in_schema.column_id("value")),
        });
    }
    Ok((
        Schema {
            fields: out_cols,
            time_index: None,
            unique_keys: Vec::new(),
            // The kept label set is runtime-only, so — unlike `by` — this does not
            // freeze the open schema to closed.
            closed: false,
        },
        sources,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[ts, value, job, host]`, the time series schema the layout tests read.
    fn series() -> Schema {
        Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("job", DataType::Utf8, false),
                col("host", DataType::Utf8, false),
            ],
            0,
            vec![],
        )
    }

    /// Each output column's source, and one source per column of
    /// `aggregate_output_schema`'s output.
    fn columns(reduction: Reduction, measures: &[AggIntent]) -> Vec<OutputColumnSource> {
        let (schema, sources) = aggregate_output(&series(), &reduction, measures, &[]).unwrap();
        assert_eq!(sources.len(), schema.fields.len());
        sources
    }

    /// `sum by (job)`: the key, then the measure.
    #[test]
    fn a_by_aggregate_keeps_its_keys_then_its_measures() {
        use OutputColumnSource::*;
        assert_eq!(
            columns(Reduction::by(vec![2]), &[AggIntent::Sum { col: None }]),
            vec![
                InputColumn(2),
                Measure {
                    index: 0,
                    input_column: Some(1)
                }
            ]
        );
    }

    /// `sum without (host)`: every label but `host`, then the measure.
    #[test]
    fn a_without_aggregate_keeps_the_other_labels_then_its_measures() {
        use OutputColumnSource::*;
        assert_eq!(
            columns(
                Reduction::Reduce(GroupKeys::without(vec![3])),
                &[AggIntent::Sum { col: None }]
            ),
            vec![
                InputColumn(2),
                Measure {
                    index: 0,
                    input_column: Some(1)
                }
            ]
        );
    }

    /// `count_values by (job) ("version", v)` and `count_values without
    /// (host) ("version", v)` both add the `version` label before the count.
    #[test]
    fn count_values_adds_its_label_by_and_without_keys() {
        use OutputColumnSource::*;
        let count_values = AggIntent::CountValues {
            label: "version".into(),
        };
        assert_eq!(
            columns(Reduction::by(vec![2]), std::slice::from_ref(&count_values)),
            vec![
                InputColumn(2),
                ComputedLabel,
                Measure {
                    index: 0,
                    input_column: Some(1)
                }
            ]
        );
        assert_eq!(
            columns(
                Reduction::Reduce(GroupKeys::without(vec![3])),
                &[count_values]
            ),
            vec![
                InputColumn(2),
                ComputedLabel,
                Measure {
                    index: 0,
                    input_column: Some(1)
                }
            ]
        );
    }

    /// `count_values("job", v)` by `job` and `without (host)`: the label
    /// replaces the kept `job` column with the value text it computes, so
    /// that position is an added `Utf8` label, not a copy of the input `job`
    /// (here nullable; the value text never is).
    #[test]
    fn a_count_values_label_replaces_a_kept_column_of_its_name() {
        use OutputColumnSource::*;
        let mut input = series();
        input.fields[2].nullable = true;
        let count_values = AggIntent::CountValues {
            label: "job".into(),
        };
        for reduction in [
            Reduction::by(vec![2]),
            Reduction::Reduce(GroupKeys::without(vec![3])),
        ] {
            let (schema, sources) =
                aggregate_output(&input, &reduction, std::slice::from_ref(&count_values), &[])
                    .unwrap();
            assert_eq!(
                sources,
                vec![
                    ComputedLabel,
                    Measure {
                        index: 0,
                        input_column: Some(1)
                    }
                ],
                "{reduction:?}"
            );
            assert_eq!(schema.fields[0], col("job", DataType::Utf8, false));
        }
    }

    /// `count_values("host", v) without (host)`: Prometheus sets the label
    /// and then removes it with the excluded ones, so no label is added and
    /// the count is per kept group.
    #[test]
    fn a_count_values_label_excluded_by_without_is_not_added() {
        use OutputColumnSource::*;
        assert_eq!(
            columns(
                Reduction::Reduce(GroupKeys::without(vec![3])),
                &[AggIntent::CountValues {
                    label: "host".into()
                }]
            ),
            vec![
                InputColumn(2),
                Measure {
                    index: 0,
                    input_column: Some(1)
                }
            ]
        );
    }

    /// `count_values("__name__", v) without (host)`: a `without` grouping
    /// always drops `__name__`, as Prometheus does, so the label is set and
    /// removed like an excluded one.
    #[test]
    fn a_count_values_name_label_under_without_is_not_added() {
        use OutputColumnSource::*;
        assert_eq!(
            columns(
                Reduction::Reduce(GroupKeys::without(vec![3])),
                &[AggIntent::CountValues {
                    label: "__name__".into()
                }]
            ),
            vec![
                InputColumn(2),
                Measure {
                    index: 0,
                    input_column: Some(1)
                }
            ]
        );
    }

    /// `sum without (host)` over `[ts, value, job, host, identity]` computes
    /// the kept series identity (its labels less `host` and `__name__`), so
    /// its source is an added label, not the input identity.
    #[test]
    fn a_without_aggregate_computes_its_kept_series_identity() {
        use OutputColumnSource::*;
        let mut input = series();
        input.fields.push(col(
            crate::ir::schema::PROMQL_SERIES_IDENTITY,
            DataType::Utf8,
            false,
        ));
        let (schema, sources) = aggregate_output(
            &input,
            &Reduction::Reduce(GroupKeys::without(vec![3])),
            &[AggIntent::Sum { col: None }],
            &[],
        )
        .unwrap();
        assert_eq!(
            schema.fields[1].name,
            crate::ir::schema::PROMQL_SERIES_IDENTITY
        );
        assert_eq!(
            sources,
            vec![
                InputColumn(2),
                ComputedLabel,
                Measure {
                    index: 0,
                    input_column: Some(1)
                }
            ]
        );
    }

    /// `absent(m{job="a", host="b"})` adds one label per matcher before its
    /// value.
    #[test]
    fn absent_adds_its_labels_before_its_value() {
        use OutputColumnSource::*;
        let absent = AggIntent::Absent {
            labels: std::collections::BTreeMap::from([
                ("host".to_string(), "b".to_string()),
                ("job".to_string(), "a".to_string()),
            ]),
        };
        assert_eq!(
            columns(Reduction::by(vec![]), &[absent]),
            vec![
                ComputedLabel,
                ComputedLabel,
                Measure {
                    index: 0,
                    input_column: Some(1)
                }
            ]
        );
    }

    /// A per-entity reduction keeps every input column, the value replaced
    /// by the measure.
    #[test]
    fn a_per_entity_reduction_replaces_the_value_by_the_measure() {
        use OutputColumnSource::*;
        assert_eq!(
            columns(Reduction::PerEntity, &[AggIntent::Rate]),
            vec![
                InputColumn(0),
                Measure {
                    index: 0,
                    input_column: Some(1)
                },
                InputColumn(2),
                InputColumn(3)
            ]
        );
    }

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

    // A nested aggregate's renamed sample value is not a kept label:
    // `sum without (inst) (sum by (inst, job) (m))` over `[inst, job, sum]`.
    #[test]
    fn without_aggregate_drops_a_renamed_sample_value() {
        let inner = Schema::new(vec![
            col("inst", DataType::Utf8, true),
            col("job", DataType::Utf8, true),
            col("sum", DataType::Float64, false),
        ]);
        let out = aggregate_output_schema(
            &inner,
            &Reduction::Reduce(GroupKeys::without(vec![0])),
            &[AggIntent::Sum { col: None }],
            &[],
        )
        .expect("valid excluded column");
        let names: Vec<_> = out.fields.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["job", "sum"]);
    }
}
