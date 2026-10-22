use super::group_table::GroupTable;
use super::*;
use crate::values::arrow_columns::ColumnDepth;
use crate::values::StateColumn;
use crate::StateColumnUpdater;
use arrow::array::{Array, BooleanArray, Float64Array, TimestampMillisecondArray, UInt32Array};
use planner_types::ir::schema::{FamilyCapability, MergeInputRule};
use std::num::NonZeroU32;

/// What each input row adds to its item's count in a keyed summary build
/// ([`Operator::per_group_keyed_summary_build`] or
/// [`Operator::all_groups_summary_build`]). Produced by the physical planner
/// from a validated `SummaryUpdate.weight` and read once per row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RowWeight {
    /// The value of this input column (Float64 or Int64); a row whose value
    /// is NULL adds nothing.
    Column(usize),
    /// This positive integer per row: 1 for a row count. The all-groups
    /// (HydraCms) build refuses one above `i32::MAX`, since
    /// asap_sketchlib's Hydra adds `i32` weights.
    Constant(NonZeroU32),
}
/// A summary evaluation: a sketch query, or an exact evaluation with typed parameters.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SummaryEvaluation {
    Sketch(planner_types::ir::schema::SketchStatistic),
    Exact(crate::summary_kernels::exact::ExactEvaluation),
}

impl Operator {
    /// One keyed summary state per group (a CMS or Count Sketch with heap):
    /// each row adds its `row_weight` to the tuple of its `items` values, in
    /// its group's state. A weight column must be Float64 or Int64, the
    /// types the kernel's `f64` weights hold; a NULL weight adds nothing.
    pub fn per_group_keyed_summary_build(
        input: SchemaRef,
        family: SummaryFamilyType,
        items: Vec<usize>,
        row_weight: RowWeight,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        use crate::summary_kernels::weighted_frequency::WeightedFrequency;
        crate::values::validate_family(&family)?;
        let SummaryFamilyType::Sketch(kind, _) = &family else {
            return Err(invalid("keyed sketch required"));
        };
        WeightedFrequency::configuration(kind)?;
        validate_groups(&input, &groups)?;
        if items.is_empty() {
            return Err(invalid("keyed summary requires item columns"));
        }
        if let RowWeight::Column(weight) = row_weight {
            if !matches!(
                plain(&input, weight)?.0,
                DataType::Float64 | DataType::Int64
            ) {
                return Err(invalid("keyed summary weights must be Float64 or Int64"));
            }
        }
        for &item in &items {
            if !matches!(
                plain(&input, item)?.0,
                DataType::Utf8
                    | DataType::Timestamp
                    | DataType::Int64
                    | DataType::Float64
                    | DataType::Bool
                    | DataType::Null
            ) {
                return Err(invalid("unsupported keyed summary identity type"));
            }
        }
        let mut fields = groups
            .iter()
            .map(|&i| input.fields[i].clone())
            .collect::<Vec<_>>();
        fields.push(SummaryField {
            name: "state".into(),
            dtype: family.clone(),
            nullable: false,
            table: None,
        });
        Ok(Self {
            kind: Kind::PerGroupKeyedSummaryBuild {
                family,
                items,
                row_weight,
                groups,
                filter: None,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    /// One summary state shared by all groups (a HydraCms grid), emitted as
    /// one row per group that reads its own part of it. Each row adds its
    /// `row_weight` to its `item` value in its group; a NULL weight adds
    /// nothing. A constant weight is at most `i32::MAX` (asap_sketchlib's
    /// Hydra adds `i32` weights).
    pub fn all_groups_summary_build(
        input: SchemaRef,
        family: SummaryFamilyType,
        item: usize,
        row_weight: RowWeight,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        crate::values::validate_family(&family)?;
        crate::factory::create_hydra_cms(&family).map_err(Error::Invalid)?;
        validate_groups(&input, &groups)?;
        if !matches!(
            plain(&input, item)?,
            (DataType::Utf8 | DataType::Int64 | DataType::Bool, false)
        ) {
            return Err(invalid(
                "HydraCms items must be non-null Utf8, Int64 or Bool",
            ));
        }
        match row_weight {
            RowWeight::Column(weight) => {
                if !matches!(
                    plain(&input, weight)?.0,
                    DataType::Float64 | DataType::Int64
                ) {
                    return Err(invalid("HydraCms weights must be Float64 or Int64"));
                }
            }
            RowWeight::Constant(weight) => {
                if i32::try_from(weight.get()).is_err() {
                    return Err(Error::Invalid(format!(
                        "HydraCms constant weight must be at most i32::MAX (asap_sketchlib's \
                         Hydra adds i32 weights), got {weight}"
                    )));
                }
            }
        }
        let mut fields = groups
            .iter()
            .map(|&i| input.fields[i].clone())
            .collect::<Vec<_>>();
        fields.push(SummaryField {
            name: "state".into(),
            dtype: family.clone(),
            nullable: false,
            table: None,
        });
        Ok(Self {
            kind: Kind::AllGroupsSummaryBuild {
                family,
                item,
                row_weight,
                groups,
                filter: None,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    /// Update the summary only from rows where `filter` is true (a filtered
    /// `SummaryAgg`). Every group still gets a state, so a group with no
    /// matching row reads as an empty summary.
    pub fn with_row_filter(mut self, filter: Expression) -> Result<Self, Error> {
        if filter.dtype(&self.inputs[0])?.0 != DataType::Bool {
            return Err(invalid("summary filter must be boolean"));
        }
        match &mut self.kind {
            Kind::PerGroupSummaryBuild { filter: slot, .. }
            | Kind::PerGroupKeyedSummaryBuild { filter: slot, .. }
            | Kind::AllGroupsSummaryBuild { filter: slot, .. } => *slot = Some(Box::new(filter)),
            _ => return Err(invalid("a row filter needs a summary build")),
        }
        Ok(self)
    }
    pub fn keyed_evaluation(
        input: SchemaRef,
        state: usize,
        k: usize,
        output: SchemaRef,
    ) -> Result<Self, Error> {
        use crate::summary_kernels::weighted_frequency::WeightedFrequency;
        crate::values::validate_family(&field(&input, state)?.dtype)?;
        let SummaryFamilyType::Sketch(kind, _) = &field(&input, state)?.dtype else {
            return Err(invalid("keyed evaluation requires summary state"));
        };
        let (_, _, _, capacity) = WeightedFrequency::configuration(kind)?;
        if k > capacity || output.fields.len() <= input.fields.len() {
            return Err(invalid("invalid keyed evaluation shape or capacity"));
        }
        if state + 1 != input.fields.len()
            || output.fields[..state] != input.fields[..state]
            || output.fields.last().unwrap().dtype != SummaryFamilyType::Plain(DataType::Float64)
        {
            return Err(invalid(
                "keyed evaluation must preserve partitions and return a Float64 score",
            ));
        }
        crate::values::validate_schema(&output)?;
        Ok(Self {
            kind: Kind::KeyedEvaluation { state, k },
            inputs: vec![input],
            output,
        })
    }
    /// One summary state per group, updated from the value of one input
    /// column per row (`time` orders counter samples): an exact aggregate,
    /// a quantile/distinct sketch, or an unkeyed frequency sketch (UnivMon)
    /// over that column's values.
    pub fn per_group_summary_build(
        input: SchemaRef,
        family: SummaryFamilyType,
        value: usize,
        time: Option<usize>,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        crate::values::validate_family(&family)?;
        validate_groups(&input, &groups)?;
        if !accepts_update_type(&family, plain(&input, value)?.0) {
            return Err(invalid("summary update type is unsupported by its family"));
        }
        if let Some(time) = time {
            if plain(&input, time)? != (&DataType::Timestamp, false) {
                return Err(invalid("summary time column must be a timestamp"));
            }
        }
        if time.is_none()
            && matches!(
                family,
                SummaryFamilyType::ExactAggregate(
                    planner_types::ir::schema::ExactKind::Rate
                        | planner_types::ir::schema::ExactKind::Increase,
                    _
                )
            )
        {
            return Err(invalid("counter summary requires a timestamp column"));
        }
        crate::capability::validate_summary_kernel(
            &family,
            &SummaryUpdate::column(ColumnRef::SampleValue),
            &Default::default(),
        )
        .map_err(Error::Invalid)?;
        let mut fields = groups
            .iter()
            .map(|&i| input.fields[i].clone())
            .collect::<Vec<_>>();
        fields.push(SummaryField {
            name: "state".into(),
            dtype: family.clone(),
            nullable: false,
            table: None,
        });
        Ok(Self {
            kind: Kind::PerGroupSummaryBuild {
                family,
                value,
                time,
                groups,
                filter: None,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    pub fn summary_merge(
        input: SchemaRef,
        state: usize,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        crate::values::validate_family(&field(&input, state)?.dtype)?;
        if matches!(field(&input, state)?.dtype, SummaryFamilyType::Plain(_)) {
            return Err(invalid("summary state required"));
        }
        let mut fields = groups
            .iter()
            .map(|&i| input.fields[i].clone())
            .collect::<Vec<_>>();
        fields.push(input.fields[state].clone());
        Ok(Self {
            kind: Kind::SummaryMerge {
                state,
                groups,
                empty_global_state: false,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    /// A merge into no groups that outputs one empty state when it has no
    /// input state: the global SQL aggregate a rollup of a relation's
    /// grouped states to no keys stands for, whose one row is NULL over no
    /// input rows.
    pub fn with_empty_global_state(mut self) -> Result<Self, Error> {
        match &mut self.kind {
            Kind::SummaryMerge {
                groups,
                empty_global_state,
                ..
            } if groups.is_empty() => *empty_global_state = true,
            _ => {
                return Err(invalid(
                    "an empty global state needs a merge into no groups",
                ))
            }
        }
        Ok(self)
    }
    pub fn evaluation(
        input: SchemaRef,
        state: usize,
        query: SummaryEvaluation,
    ) -> Result<Self, Error> {
        let family = &field(&input, state)?.dtype;
        crate::values::validate_family(family)?;
        match &query {
            SummaryEvaluation::Sketch(query) => {
                crate::capability::validate_sketch_evaluation(family, query)?
            }
            SummaryEvaluation::Exact(evaluation) => {
                crate::capability::validate_exact_evaluation(family, evaluation)?
            }
        }
        let mut fields = input.fields.clone();
        let result_type = if matches!(
            fields[state].dtype,
            SummaryFamilyType::ExactAggregate(planner_types::ir::schema::ExactKind::Count, _)
        ) || integral_count(family, &query)
        {
            DataType::Int64
        } else {
            DataType::Float64
        };
        // A state-only row represents the global population. Its extrema may
        // be empty, just like an ordinary ungrouped MIN/MAX aggregate.
        let nullable = fields.len() == 1
            && matches!(
                fields[state].dtype,
                SummaryFamilyType::ExactAggregate(
                    planner_types::ir::schema::ExactKind::Min
                        | planner_types::ir::schema::ExactKind::Max,
                    _
                )
            );
        fields[state] = result_field("value", result_type, nullable);
        Ok(Self {
            kind: Kind::Evaluation { state, query },
            inputs: vec![input],
            output: schema(fields),
        })
    }
}
/// Whether a per-group build of `family` takes values of `dtype`: every
/// family takes Float64; an exact count any type (it counts values); a
/// sketch also takes Int64 (a numeric sketch as its exact Float64, a
/// distinct count as an integer item); a distinct count (KMV, HLL) also
/// takes Utf8, and UnivMon Utf8 and Bool.
fn accepts_update_type(family: &SummaryFamilyType, dtype: &DataType) -> bool {
    use planner_types::ir::schema::{ExactKind, SketchAlgorithm as A};
    let SummaryFamilyType::Sketch(kind, _) = family else {
        return *dtype == DataType::Float64
            || matches!(
                family,
                SummaryFamilyType::ExactAggregate(ExactKind::Count, _)
            );
    };
    match dtype {
        DataType::Float64 | DataType::Int64 => true,
        DataType::Utf8 => matches!(kind.algorithm(), A::Kmv | A::Hll | A::UnivMon),
        DataType::Bool => kind.algorithm() == &A::UnivMon,
        _ => false,
    }
}

impl Operator {
    /// This distinct-count evaluation ([`Self::evaluation`] of
    /// `Cardinality`) with an Int64 value, its estimate rounded to the
    /// nearest whole number: SQL `COUNT(DISTINCT)` is Int64, while a PromQL
    /// sample value stays Float64.
    pub fn with_whole_distinct_count(mut self) -> Result<Self, Error> {
        let Kind::Evaluation {
            state,
            query:
                SummaryEvaluation::Sketch(planner_types::ir::schema::SketchStatistic::Cardinality),
        } = &self.kind
        else {
            return Err(invalid(
                "a whole distinct count requires a cardinality evaluation",
            ));
        };
        let mut fields = self.output.fields.clone();
        let nullable = fields[*state].nullable;
        fields[*state] = result_field("value", DataType::Int64, nullable);
        self.output = schema(fields);
        Ok(self)
    }
}

/// The Planner reads a Count-Min bare count only for count intents, whose
/// output is Int64 and whose updates have unit weight; execution rejects a
/// non-integral total rather than rounding it.
fn integral_count(family: &SummaryFamilyType, query: &SummaryEvaluation) -> bool {
    matches!(family, SummaryFamilyType::Sketch(kind, _)
        if kind.algorithm() == &planner_types::ir::schema::SketchAlgorithm::Cms)
        && matches!(
            query,
            SummaryEvaluation::Sketch(planner_types::ir::schema::SketchStatistic::PointCount {
                value: None,
                ..
            })
        )
}
pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let range_ms = operator.evaluation_range(&context)?;
    let output = operator.output.clone();
    let input_schema = operator.inputs[0].clone();
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    match &operator.kind {
        Kind::PerGroupSummaryBuild {
            family,
            value,
            time,
            groups,
            filter,
        } => Ok(futures::stream::once(async move {
            build_per_group_summary(
                input,
                PerGroupBuild {
                    family,
                    value: *value,
                    time: *time,
                    groups,
                    filter: filter.as_deref(),
                    emit_empty_global: !input_schema.has_promql_series_identity(),
                },
                &input_schema,
                &output,
                &context,
            )
            .await
        })
        .boxed_local()),
        Kind::PerGroupKeyedSummaryBuild {
            family,
            items,
            row_weight,
            groups,
            filter,
        } => Ok(futures::stream::once(async move {
            build_per_group_keyed_summary(
                input,
                KeyedBuild {
                    family,
                    items,
                    row_weight: *row_weight,
                    groups,
                    filter: filter.as_deref(),
                },
                &input_schema,
                &output,
                &context,
            )
            .await
        })
        .boxed_local()),
        Kind::AllGroupsSummaryBuild {
            family,
            item,
            row_weight,
            groups,
            filter,
        } => Ok(futures::stream::once(async move {
            build_all_groups_summary(
                input,
                AllGroupsBuild {
                    family,
                    item: *item,
                    row_weight: *row_weight,
                    groups,
                    filter: filter.as_deref(),
                },
                &input_schema,
                &output,
                &context,
            )
            .await
        })
        .boxed_local()),
        Kind::KeyedEvaluation { state, k } => Ok(input
            .map(move |batch| keyed_evaluation(&*batch?, *state, *k, &output))
            .boxed_local()),
        Kind::Evaluation { state, query } => Ok(input
            .map(move |batch| evaluate_states(&*batch?, *state, query, range_ms, &output))
            .boxed_local()),
        _ => unreachable!(),
    }
}
pub(super) fn execute_merge<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let output = operator.output.clone();
    let input_schema = operator.inputs[0].clone();
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    Ok(futures::stream::once(async move {
        let Kind::SummaryMerge {
            state,
            groups,
            empty_global_state,
        } = &operator.kind
        else {
            unreachable!()
        };
        let merge = Merge {
            state_column: *state,
            groups,
            empty_global_state: *empty_global_state,
        };
        merge_summary(input, merge, &input_schema, &output, &context).await
    })
    .boxed_local())
}

/// The rows a summary build's filter keeps: only a true predicate does, as
/// with SQL `FILTER (WHERE …)`. `None` keeps every row.
fn filter_mask(filter: Option<&Expression>, batch: &Batch) -> Result<Option<BooleanArray>, Error> {
    filter
        .map(|filter| {
            let mask = filter.evaluate_batch(batch, &DataType::Bool)?;
            mask.as_any()
                .downcast_ref::<BooleanArray>()
                .cloned()
                .ok_or_else(|| invalid("summary filter must be boolean"))
        })
        .transpose()
}

fn kept(mask: &Option<BooleanArray>, row: usize) -> bool {
    mask.as_ref()
        .is_none_or(|mask| mask.is_valid(row) && mask.value(row))
}

/// The output batch of a build or merge: the groups' labels in key order,
/// then their states.
fn grouped_states(
    table: &GroupTable,
    states: StateColumn,
    input: &SchemaRef,
    output: &SchemaRef,
) -> Result<Batch, Error> {
    let order = table.groups_in_key_order();
    let mut arrays = table.label_arrays(input, &order)?;
    let mut columns = vec![None; arrays.len()];
    arrays.push(Arc::new(UInt32Array::from(order.clone())));
    columns.push(Some(Arc::new(states)));
    Batch::from_parts(output.clone(), arrays, columns, order.len())
}

/// A `PerGroupSummaryBuild`'s parameters.
struct PerGroupBuild<'a> {
    family: &'a SummaryFamilyType,
    value: usize,
    time: Option<usize>,
    groups: &'a [usize],
    filter: Option<&'a Expression>,
    /// Whether an empty input still has the global group (SQL; not PromQL).
    emit_empty_global: bool,
}

/// One state per group, updated row by row through the family's
/// [`StateColumnUpdater`]; a run-local [`GroupTable`] numbers the groups.
async fn build_per_group_summary(
    mut input: Input<'_, Batch>,
    build: PerGroupBuild<'_>,
    input_schema: &SchemaRef,
    output: &SchemaRef,
    context: &RunContext,
) -> Result<Batch, Error> {
    // `now()` in the filter is this run's evaluation time.
    let filter = build
        .filter
        .map(|filter| filter.at_evaluation_time(context));
    let mut table = GroupTable::new(build.groups.to_vec());
    let mut updater = StateColumnUpdater::new(build.family)?;
    let mut work = Cooperative::new(context);
    let mut workspace = Workspace::new(context)?;
    // PromQL aggregation of an empty vector produces no sample.
    if build.groups.is_empty() && build.emit_empty_global {
        table.add_empty_group()?;
        updater.add_group()?;
        workspace.set_bytes(updater.memory_bytes() + table.memory_bytes())?;
    }
    while let Some(whole) = input.next().await {
        let whole = whole?;
        // A bounded number of rows at a time: memory is checked after each
        // chunk's new groups, so one large batch cannot overshoot the limit
        // by more than a chunk's states.
        for batch in row_chunks(&whole) {
            let groups = table.group_ids(&batch)?;
            updater.ensure_groups(table.len())?;
            workspace.set_bytes(updater.memory_bytes() + table.memory_bytes())?;
            let mask = filter_mask(filter.as_deref(), &batch)?;
            let times = build.time.map(|time| {
                batch
                    .column(time)
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .expect("summary time columns are timestamps")
                    .clone()
            });
            let time_at = |row: usize| times.as_ref().map_or(0, |times| times.value(row));
            let values = batch.column(build.value);
            if let Some(values) = values.as_any().downcast_ref::<Float64Array>() {
                for (row, &group) in groups.iter().enumerate() {
                    // SQL aggregates ignore NULL samples while retaining the group.
                    if !kept(&mask, row) || values.is_null(row) {
                        continue;
                    }
                    updater.update(group, values.value(row), time_at(row))?;
                }
            } else if let Some(values) = values.as_any().downcast_ref::<arrow::array::Int64Array>()
            {
                // Int64 ids or numbers, without decoding the column.
                for (row, &group) in groups.iter().enumerate() {
                    if !kept(&mask, row) || values.is_null(row) {
                        continue;
                    }
                    updater.update_value(group, &Value::Int64(values.value(row)), time_at(row))?;
                }
            } else {
                // Typed values: Utf8 items of a distinct count (KMV, HLL)
                // or of UnivMon, and Bool items of UnivMon.
                let SummaryFamilyType::Plain(dtype) = &input_schema.fields[build.value].dtype
                else {
                    return Err(invalid("summary values are plain"));
                };
                let values = crate::values::arrow_columns::decode(
                    values.as_ref(),
                    dtype,
                    ColumnDepth::BatchColumn,
                );
                for (row, &group) in groups.iter().enumerate() {
                    if !kept(&mask, row) || matches!(values[row], Value::Null) {
                        continue;
                    }
                    updater.update_value(group, &values[row], time_at(row))?;
                }
            }
            work.checkpoint_rows(batch.num_rows()).await?;
            workspace.set_bytes(updater.memory_bytes() + table.memory_bytes())?;
        }
    }
    grouped_states(&table, updater.seal(), input_schema, output)
}

/// A `PerGroupKeyedSummaryBuild`'s parameters.
struct KeyedBuild<'a> {
    family: &'a SummaryFamilyType,
    items: &'a [usize],
    row_weight: RowWeight,
    groups: &'a [usize],
    filter: Option<&'a Expression>,
}

/// The weight each row adds in a keyed build.
enum RowWeights {
    /// The same weight for every row.
    Constant(NonZeroU32),
    /// Each row's weight column value; NULL adds nothing.
    Column(Vec<Value>),
}

impl RowWeights {
    fn of(batch: &Batch, row_weight: RowWeight) -> Self {
        match row_weight {
            RowWeight::Constant(weight) => Self::Constant(weight),
            RowWeight::Column(column) => {
                let SummaryFamilyType::Plain(dtype) = &batch.schema().fields[column].dtype else {
                    unreachable!("validated weight column")
                };
                Self::Column(crate::values::arrow_columns::decode(
                    batch.column(column).as_ref(),
                    dtype,
                    ColumnDepth::BatchColumn,
                ))
            }
        }
    }
}

/// The decoded values of `columns` of `batch`, column by column.
fn decoded_columns(batch: &Batch, columns: &[usize]) -> Vec<Vec<Value>> {
    columns
        .iter()
        .map(|&column| {
            let SummaryFamilyType::Plain(dtype) = &batch.schema().fields[column].dtype else {
                unreachable!("validated item column")
            };
            crate::values::arrow_columns::decode(
                batch.column(column).as_ref(),
                dtype,
                ColumnDepth::BatchColumn,
            )
        })
        .collect()
}

/// One keyed state per group: each kept row adds its weight to the tuple of
/// its item values in its group's state.
async fn build_per_group_keyed_summary(
    mut input: Input<'_, Batch>,
    build: KeyedBuild<'_>,
    input_schema: &SchemaRef,
    output: &SchemaRef,
    context: &RunContext,
) -> Result<Batch, Error> {
    // `now()` in the filter is this run's evaluation time.
    let filter = build
        .filter
        .map(|filter| filter.at_evaluation_time(context));
    let mut table = GroupTable::new(build.groups.to_vec());
    let mut updater = StateColumnUpdater::new(build.family)?;
    let mut work = Cooperative::new(context);
    let mut workspace = Workspace::new(context)?;
    let mut items = Vec::with_capacity(build.items.len());
    while let Some(whole) = input.next().await {
        let whole = whole?;
        for batch in row_chunks(&whole) {
            let groups = table.group_ids(&batch)?;
            updater.ensure_groups(table.len())?;
            workspace.set_bytes(updater.memory_bytes() + table.memory_bytes())?;
            let mask = filter_mask(filter.as_deref(), &batch)?;
            let weights = RowWeights::of(&batch, build.row_weight);
            let columns = decoded_columns(&batch, build.items);
            for (row, &group) in groups.iter().enumerate() {
                if !kept(&mask, row) {
                    continue;
                }
                let weight = match &weights {
                    RowWeights::Constant(weight) => f64::from(weight.get()),
                    RowWeights::Column(values) => match &values[row] {
                        Value::Null => continue,
                        weight => keyed_weight(weight)?,
                    },
                };
                items.clear();
                // The kernel keys epoch-millisecond timestamps as Int64.
                items.extend(columns.iter().map(|column| match &column[row] {
                    Value::Timestamp(time) => Value::Int64(*time),
                    value => value.clone(),
                }));
                updater.update_item(group, &items, weight)?;
            }
            work.checkpoint_rows(batch.num_rows()).await?;
            workspace.set_bytes(updater.memory_bytes() + table.memory_bytes())?;
        }
    }
    grouped_states(&table, updater.seal(), input_schema, output)
}

/// An `AllGroupsSummaryBuild`'s parameters.
struct AllGroupsBuild<'a> {
    family: &'a SummaryFamilyType,
    item: usize,
    row_weight: RowWeight,
    groups: &'a [usize],
    filter: Option<&'a Expression>,
}

/// One HydraCms grid shared by all groups; each group's state reads its own
/// part of it, named by an injective encoding of its typed key.
async fn build_all_groups_summary(
    mut input: Input<'_, Batch>,
    build: AllGroupsBuild<'_>,
    input_schema: &SchemaRef,
    output: &SchemaRef,
    context: &RunContext,
) -> Result<Batch, Error> {
    // `now()` in the filter is this run's evaluation time.
    let filter = build
        .filter
        .map(|filter| filter.at_evaluation_time(context));
    use crate::summary_kernels::HydraCmsGroup;
    use crate::AggregateCore;
    let mut grid = crate::factory::create_hydra_cms(build.family).map_err(Error::Invalid)?;
    let grid_bytes = grid.approx_memory_bytes();
    let mut workspace = Workspace::new(context)?;
    workspace.set_bytes(grid_bytes)?;
    let mut work = Cooperative::new(context);
    let mut table = GroupTable::new(build.groups.to_vec());
    // Each group's Hydra subpopulation name.
    let mut names: Vec<String> = Vec::new();
    let mut name_bytes = 0;
    while let Some(whole) = input.next().await {
        let whole = whole?;
        for batch in row_chunks(&whole) {
            let groups = table.group_ids(&batch)?;
            for group in names.len()..table.len() {
                let key = (0..build.groups.len())
                    .map(|index| table.label(group as u32, index).clone())
                    .collect::<Vec<_>>();
                let name = crate::summary_kernels::hydra_cms::hydra_group_name(&key)?;
                name_bytes += name.len();
                names.push(name);
            }
            workspace.set_bytes(grid_bytes + table.memory_bytes() + name_bytes)?;
            let mask = filter_mask(filter.as_deref(), &batch)?;
            let weights = RowWeights::of(&batch, build.row_weight);
            let items = decoded_columns(&batch, &[build.item]).remove(0);
            for (row, &group) in groups.iter().enumerate() {
                if !kept(&mask, row) {
                    continue;
                }
                let weight = match &weights {
                    RowWeights::Constant(weight) => {
                        i32::try_from(weight.get()).expect("validated HydraCms constant weight")
                    }
                    RowWeights::Column(values) => match &values[row] {
                        Value::Null => continue,
                        weight => crate::summary_kernels::hydra_cms::hydra_weight(weight)?,
                    },
                };
                let item = crate::summary_kernels::hydra_cms::hydra_item(&items[row])?;
                grid.count(&names[group as usize], &item, weight)
                    .map_err(|e| Error::Operator(e.to_string()))?;
            }
            work.checkpoint_rows(batch.num_rows()).await?;
        }
    }
    let grid = Arc::new(grid);
    let states = names
        .into_iter()
        .map(|group| {
            Arc::new(HydraCmsGroup {
                grid: grid.clone(),
                group,
            }) as Arc<dyn AggregateCore>
        })
        .collect();
    grouped_states(
        &table,
        StateColumn::from_objects(build.family.clone(), states),
        input_schema,
        output,
    )
}

/// The state indices of a summary column: row `i`'s state is `indices[i]`.
fn state_indices(batch: &Batch, state: usize) -> Result<(&StateColumn, &UInt32Array), Error> {
    let states = batch
        .state_column(state)
        .ok_or_else(|| invalid("summary value required"))?;
    let indices = batch
        .column(state)
        .as_any()
        .downcast_ref::<UInt32Array>()
        .expect("summary columns hold state indices");
    Ok((states, indices))
}

/// Merge the states of each group across all input rows.
/// A `SummaryMerge`'s parameters.
struct Merge<'a> {
    state_column: usize,
    groups: &'a [usize],
    empty_global_state: bool,
}

async fn merge_summary(
    mut input: Input<'_, Batch>,
    merge: Merge<'_>,
    input_schema: &SchemaRef,
    output: &SchemaRef,
    context: &RunContext,
) -> Result<Batch, Error> {
    use crate::summary_kernels::exact::ScalarState;
    use crate::values::StateStorage;
    let Merge {
        state_column,
        groups,
        empty_global_state,
    } = merge;
    let family = &input_schema.fields[state_column].dtype;
    let mut batches = Vec::new();
    let mut reservations = Vec::new();
    while let Some(batch) = input.next().await {
        let batch = batch?;
        reservations.push(context.reserve(batch.memory_bytes())?);
        batches.push((*batch).clone());
    }
    let mut table = GroupTable::new(groups.to_vec());
    let mut work = Cooperative::new(context);
    // Every input row: its batch, its row, its group.
    let mut entries = Vec::new();
    for (index, batch) in batches.iter().enumerate() {
        let ids = table.group_ids(batch)?;
        entries.extend(
            ids.into_iter()
                .enumerate()
                .map(|(row, group)| (index, row, group)),
        );
    }
    // A global SQL aggregate's one row over no input rows: one empty state.
    if entries.is_empty() && empty_global_state {
        table.add_empty_group()?;
        let mut updater = StateColumnUpdater::new(family)?;
        updater.add_group()?;
        return grouped_states(&table, updater.seal(), input_schema, output);
    }
    let columns = batches
        .iter()
        .map(|batch| state_indices(batch, state_column))
        .collect::<Result<Vec<_>, _>>()?;
    let state_of = |(batch, row, _): (usize, usize, u32)| {
        let (states, indices) = columns[batch];
        (states, indices.value(row) as usize)
    };
    // Counter states join consecutive windows pairwise, which is exact only
    // in time order: order the inputs by their window start.
    if FamilyCapability::of(family).merge_inputs == Some(MergeInputRule::DisjointInTime) {
        entries.sort_by_cached_key(|&entry| {
            let (states, index) = state_of(entry);
            match states.storage() {
                StateStorage::Exact(records) => records[index].earliest_sample_ms(),
                StateStorage::Objects(objects) => objects[index]
                    .as_any()
                    .downcast_ref::<crate::summary_kernels::exact::ExactAccumulator>()
                    .and_then(|state| state.earliest_sample_ms()),
            }
        });
    }
    let all_exact = columns
        .iter()
        .all(|(states, _)| matches!(states.storage(), StateStorage::Exact(_)));
    if all_exact {
        // Exact records merge in place, one fixed-size record per group.
        let mut merged: Vec<Option<ScalarState>> = vec![None; table.len()];
        let mut workspace = Workspace::new(context)?;
        workspace.set_bytes(
            merged.len() * std::mem::size_of::<Option<ScalarState>>() + table.memory_bytes(),
        )?;
        for chunk in entries.chunks(ROWS_PER_CHUNK) {
            for &entry in chunk {
                let (states, index) = state_of(entry);
                let StateStorage::Exact(records) = states.storage() else {
                    unreachable!("all exact")
                };
                let slot = &mut merged[entry.2 as usize];
                *slot = Some(match slot.take() {
                    None => records[index].clone(),
                    Some(existing) => existing
                        .merge(&records[index])
                        .map_err(|e| Error::Operator(e.to_string()))?,
                });
            }
            work.checkpoint_rows(chunk.len()).await?;
        }
        let records = merged
            .into_iter()
            .map(|record| record.expect("every group has a row"))
            .collect();
        return grouped_states(
            &table,
            StateColumn::from_exact(family.clone(), records),
            input_schema,
            output,
        );
    }
    let merged = merge_objects(
        &entries,
        &state_of,
        table.len(),
        table.memory_bytes(),
        context,
        &mut work,
    )
    .await?;
    grouped_states(
        &table,
        StateColumn::from_objects_of(family.clone(), merged),
        input_schema,
        output,
    )
}

/// Merge kernel-object states per group, in `entries` order. Each distinct
/// pair of Hydra grids merges once, and every group of the pair reads the
/// one merged grid. Each output state comes with whether something outside
/// the run holds it: a state passed through unchanged keeps its input's
/// bit (a stored state is shared by pointer, not charged again); a merged
/// state is new.
async fn merge_objects<'b>(
    entries: &[(usize, usize, u32)],
    state_of: &dyn Fn((usize, usize, u32)) -> (&'b StateColumn, usize),
    group_count: usize,
    table_memory_bytes: usize,
    context: &RunContext,
    work: &mut Cooperative,
) -> Result<Vec<(Arc<dyn crate::AggregateCore>, bool)>, Error> {
    use crate::summary_kernels::{HydraCms, HydraCmsGroup};
    let mut merged: Vec<Option<(Arc<dyn crate::AggregateCore>, bool)>> = vec![None; group_count];
    // The group table is held while the states merge.
    let mut memory = context.reserve(table_memory_bytes)?;
    let mut retained = table_memory_bytes;
    // States shared by several groups (Hydra grids) are reserved once each.
    let mut shared = BTreeMap::<usize, usize>::new();
    // The inputs are kept so their addresses stay unique while they key the memo.
    type GridPair = (Arc<HydraCms>, Arc<HydraCms>, Arc<HydraCms>);
    let mut grids = BTreeMap::<(usize, usize), GridPair>::new();
    for &entry in entries {
        work.checkpoint().await?;
        let (states, index) = state_of(entry);
        let state = states.state(index);
        let held_outside_run = states.state_held_outside_run(index);
        // A stored state, and the grid it shares, are held outside the run.
        if let Some((id, bytes)) = state.shared_state().filter(|_| !held_outside_run) {
            if shared.insert(id, bytes).is_none() {
                retained = retained.checked_add(bytes).ok_or(Error::MemoryLimit)?;
                memory.resize(retained)?;
            }
        }
        let slot = &mut merged[entry.2 as usize];
        let Some((existing, existing_held_outside_run)) = slot else {
            // Passed through: its slot, and the state unless stored.
            let own_bytes = if held_outside_run {
                0
            } else {
                state.approx_memory_bytes()
            };
            retained = retained
                .checked_add(own_bytes + 64)
                .ok_or(Error::MemoryLimit)?;
            memory.resize(retained)?;
            *slot = Some((state, held_outside_run));
            continue;
        };
        // Whatever follows replaces the slot's state with a new one.
        let was_held_outside_run = std::mem::replace(existing_held_outside_run, false);
        if let (Some(left), Some(right)) = (
            existing.as_any().downcast_ref::<HydraCmsGroup>(),
            state.as_any().downcast_ref::<HydraCmsGroup>(),
        ) {
            if left.group != right.group {
                return Err(invalid("HydraCms merge requires the same group"));
            }
            let pair = (
                Arc::as_ptr(&left.grid) as usize,
                Arc::as_ptr(&right.grid) as usize,
            );
            let grid = match grids.entry(pair) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.get().2.clone(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let grid = Arc::new(
                        left.grid
                            .merge(&right.grid)
                            .map_err(|e| Error::Operator(e.to_string()))?,
                    );
                    let bytes = grid.approx_memory_bytes();
                    shared.insert(Arc::as_ptr(&grid) as usize, bytes);
                    retained = retained.checked_add(bytes).ok_or(Error::MemoryLimit)?;
                    memory.resize(retained)?;
                    entry
                        .insert((left.grid.clone(), right.grid.clone(), grid))
                        .2
                        .clone()
                }
            };
            *existing = Arc::new(HydraCmsGroup {
                grid,
                group: left.group.clone(),
            });
            continue;
        }
        // A stored state was charged only its slot.
        let old_bytes = if was_held_outside_run {
            0
        } else {
            existing.approx_memory_bytes()
        };
        // Reserve an estimate for the replacement while both input states remain live.
        memory.resize(
            retained
                .checked_add(old_bytes)
                .and_then(|n| n.checked_add(state.approx_memory_bytes()))
                .ok_or(Error::MemoryLimit)?,
        )?;
        *existing = Arc::from(
            existing
                .merge_with(state.as_ref())
                .map_err(|e| Error::Operator(e.to_string()))?,
        );
        retained = retained
            .checked_sub(old_bytes)
            .and_then(|n| n.checked_add(existing.approx_memory_bytes()))
            .ok_or(Error::MemoryLimit)?;
        memory.resize(retained)?;
    }
    Ok(merged
        .into_iter()
        .map(|state| state.expect("every group has a row"))
        .collect())
}

/// `SummaryEstimate` / `FinalizeExactAccumulator`: each row's state read
/// as one number, in place of the state column. A counter with fewer than
/// two samples drops its row (PromQL omits the series).
fn evaluate_states(
    batch: &Batch,
    state: usize,
    query: &SummaryEvaluation,
    range_ms: Option<(i64, i64)>,
    output: &SchemaRef,
) -> Result<Batch, Error> {
    use crate::summary_kernels::exact::{statistic_of, ExactAccumulator, ScalarState};
    use crate::values::StateStorage;
    let (states, indices) = state_indices(batch, state)?;
    let field = &output.fields[state];
    let integral = field.dtype == SummaryFamilyType::Plain(DataType::Int64);
    let nullable = field.nullable;
    // An exact state's value, from its record.
    let exact_value = |record: &ScalarState,
                       evaluation: &crate::summary_kernels::exact::ExactEvaluation|
     -> Result<Option<Value>, Error> {
        if nullable && record.is_empty_sum() {
            return Ok(None);
        }
        if integral {
            let count = record.count().ok_or_else(|| {
                Error::Operator("exact count state lacks an integer count".into())
            })?;
            return Ok(Some(Value::Int64(i64::try_from(count).map_err(|_| {
                Error::Operator("exact count exceeds Int64".into())
            })?)));
        }
        if evaluation.statistic != statistic_of(states.family()) {
            return Err(Error::Operator(
                "evaluation differs from Planner exact family".into(),
            ));
        }
        match record
            .evaluation(evaluation.statistic, range_ms)
            .map_err(|e| Error::Operator(e.to_string()))?
        {
            Some(value) => Ok(Some(Value::Float64(value))),
            None if nullable => Ok(None),
            None => Err(Error::Operator("empty exact population".into())),
        }
    };
    // A counter with too few samples drops its row.
    let insufficient = |index: usize| -> bool {
        let SummaryEvaluation::Exact(evaluation) = query else {
            return false;
        };
        if !matches!(
            evaluation.statistic,
            crate::Statistic::Rate | crate::Statistic::Increase
        ) {
            return false;
        }
        match states.storage() {
            StateStorage::Exact(records) => {
                evaluation.statistic == statistic_of(states.family())
                    && records[index].insufficient_counter_raw_data_samples()
            }
            StateStorage::Objects(objects) => {
                crate::evaluation::insufficient_counter_raw_data_samples(
                    objects[index].as_ref(),
                    evaluation.statistic,
                )
            }
        }
    };
    let mut keep = Vec::with_capacity(batch.num_rows());
    let mut values: Vec<Option<Value>> = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let index = indices.value(row) as usize;
        if insufficient(index) {
            keep.push(false);
            continue;
        }
        keep.push(true);
        values.push(match (query, states.storage()) {
            (SummaryEvaluation::Exact(evaluation), StateStorage::Exact(records)) => {
                exact_value(&records[index], evaluation)?
            }
            (SummaryEvaluation::Exact(evaluation), StateStorage::Objects(objects)) => {
                let exact = objects[index]
                    .as_any()
                    .downcast_ref::<ExactAccumulator>()
                    .ok_or_else(|| invalid("exact evaluation requires exact state"))?;
                let record = exact
                    .scalar()
                    .ok_or_else(|| invalid("exact evaluation requires an unkeyed state"))?;
                exact_value(record, evaluation)?
            }
            (SummaryEvaluation::Sketch(query), storage) => {
                let summary = match storage {
                    StateStorage::Objects(objects) => objects[index].clone(),
                    StateStorage::Exact(_) => states.state(index),
                };
                // A filtered or NULL-only group's quantile is SQL NULL.
                if nullable && summary.is_empty() {
                    None
                } else {
                    let value = summary
                        .estimate(query)
                        .map_err(|e| Error::Operator(e.to_string()))?;
                    if integral && *query == planner_types::ir::schema::SketchStatistic::Cardinality
                    {
                        // A distinct count is a whole number: the estimate
                        // rounded to the nearest one.
                        let rounded = value.round();
                        if !(0.0..9.223_372_036_854_775e18).contains(&rounded) {
                            return Err(Error::Operator(format!(
                                "distinct count estimate {value} is not an Int64"
                            )));
                        }
                        Some(Value::Int64(rounded as i64))
                    } else if integral {
                        // Below 2^53 an f64 sum of unit updates is exact.
                        if value.fract() != 0.0 || !(0.0..9.007_199_254_740_992e15).contains(&value)
                        {
                            return Err(Error::Operator(
                                "Count-Min count is not an exact integer".into(),
                            ));
                        }
                        Some(Value::Int64(value as i64))
                    } else {
                        Some(Value::Float64(value))
                    }
                }
            }
        });
    }
    let kept = if keep.iter().all(|&k| k) {
        batch.clone()
    } else {
        batch.filter(&BooleanArray::from(keep))?
    };
    let mut arrays = kept.record_batch().columns().to_vec();
    let mut columns = (0..arrays.len())
        .map(|column| kept.state_column(column).cloned())
        .collect::<Vec<_>>();
    let SummaryFamilyType::Plain(dtype) = &field.dtype else {
        unreachable!("an evaluation outputs a plain value")
    };
    let cells = values
        .iter()
        .map(|value| value.as_ref().unwrap_or(&Value::Null))
        .collect::<Vec<_>>();
    arrays[state] = crate::values::arrow_columns::encode(dtype, ColumnDepth::BatchColumn, &cells)?;
    columns[state] = None;
    Batch::from_parts(output.clone(), arrays, columns, kept.num_rows())
}

/// `SummaryEstimate` of a top-k: each input row's `k` heaviest items, one
/// output row per item, after the row's own columns.
fn keyed_evaluation(
    batch: &Batch,
    state: usize,
    k: usize,
    output: &SchemaRef,
) -> Result<Batch, Error> {
    let (states, indices) = state_indices(batch, state)?;
    let mut source_rows = Vec::new();
    let mut item_columns: Vec<Vec<Value>> = vec![Vec::new(); output.fields.len() - state];
    for row in 0..batch.num_rows() {
        let summary = states.state(indices.value(row) as usize);
        let summary = summary
            .as_any()
            .downcast_ref::<crate::summary_kernels::weighted_frequency::WeightedFrequency>()
            .ok_or_else(|| invalid("weighted frequency typed state required"))?;
        for items in summary.rows(k) {
            source_rows.push(row as u32);
            for ((column, value), field) in item_columns
                .iter_mut()
                .zip(items)
                .zip(&output.fields[state..])
            {
                // The typed output schema restores epoch-millisecond
                // timestamp keys from the kernel's Int64 representation.
                column.push(match value {
                    Value::Int64(time)
                        if field.dtype == SummaryFamilyType::Plain(DataType::Timestamp) =>
                    {
                        Value::Timestamp(time)
                    }
                    value => value,
                });
            }
        }
    }
    let take = UInt32Array::from(source_rows);
    let mut arrays = Vec::with_capacity(output.fields.len());
    let mut columns = Vec::with_capacity(output.fields.len());
    for column in 0..state {
        arrays.push(
            arrow::compute::take(batch.column(column).as_ref(), &take, None)
                .map_err(|e| Error::Operator(e.to_string()))?,
        );
        columns.push(batch.state_column(column).cloned());
    }
    for (values, field) in item_columns.iter().zip(&output.fields[state..]) {
        let SummaryFamilyType::Plain(dtype) = &field.dtype else {
            return Err(invalid("top-k items are plain"));
        };
        arrays.push(crate::values::arrow_columns::encode(
            dtype,
            ColumnDepth::BatchColumn,
            &values.iter().collect::<Vec<_>>(),
        )?);
        columns.push(None);
    }
    Batch::from_parts(output.clone(), arrays, columns, take.len())
}

/// One row's weight-column value as the keyed kernel's `f64` weight. An
/// Int64 weight beyond 2^53 has no exact `f64` and is refused, not rounded.
fn keyed_weight(value: &Value) -> Result<f64, Error> {
    match value {
        Value::Float64(v) => Ok(*v),
        Value::Int64(v) if v.unsigned_abs() <= 1 << 53 => Ok(*v as f64),
        Value::Int64(v) => Err(Error::Operator(format!(
            "keyed summary weight {v} has no exact Float64"
        ))),
        _ => Err(Error::Operator("keyed summary weight type".into())),
    }
}
