use super::group_table::GroupTable;
use super::*;
use crate::values::arrow_columns::ColumnDepth;
use crate::values::arrow_columns::{float64_array, timestamp_array};
use crate::values::StateColumn;
use crate::StateColumnUpdater;
use arrow::array::{Array, BooleanArray, UInt32Array};
use planner_types::summary_type::{MergeInputRule, SummaryTypeCapability};
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
    Sketch(planner_types::summary_type::SketchStatistic),
    Exact(crate::summary_kernels::exact::ExactEvaluation),
}

impl Operator {
    /// One keyed summary state per group (a CMS or Count Sketch with heap)
    /// of θ `parameters`: each row adds its `row_weight` to the tuple of its
    /// `items` values, in its group's state. A weight column must be Float64
    /// or Int64, the types the kernel's `f64` weights hold; a NULL weight
    /// adds nothing.
    pub fn per_group_keyed_summary_build(
        input: SchemaRef,
        family: SummaryFamilyType,
        parameters: Option<SketchParams>,
        items: Vec<usize>,
        row_weight: RowWeight,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        use crate::summary_kernels::weighted_frequency::WeightedFrequency;
        use planner_types::summary_type::SketchAlgorithm as A;
        crate::values::validate_family(&family)?;
        let SummaryFamilyType::Sketch(algorithm @ (A::CmsWithHeap | A::CountSketchWithHeap)) =
            &family
        else {
            return Err(invalid("keyed sketch required"));
        };
        let Some(theta) = &parameters else {
            return Err(invalid("a keyed sketch needs its parameters θ"));
        };
        WeightedFrequency::configuration(algorithm, theta)?;
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
        });
        Ok(Self {
            kind: Kind::PerGroupKeyedSummaryBuild {
                family,
                parameters,
                items,
                row_weight,
                groups,
                filter: None,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    /// One summary state shared by all groups (a HydraCms grid sized by θ
    /// `parameters`, `SketchParams::Hydra(HydraParams::HydraCms { .. })`),
    /// emitted as one row per group that reads its own part of it. Each row adds its
    /// `row_weight` to its `item` value in its group; a NULL weight adds
    /// nothing. A constant weight is at most `i32::MAX` (asap_sketchlib's
    /// Hydra adds `i32` weights).
    pub fn all_groups_summary_build(
        input: SchemaRef,
        family: SummaryFamilyType,
        parameters: Option<SketchParams>,
        item: usize,
        row_weight: RowWeight,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        crate::values::validate_family(&family)?;
        // The grid's shape only: the grid itself is allocated when it runs.
        crate::capability::hydra_cms_shape(&family, parameters.as_ref()).map_err(Error::Invalid)?;
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
        });
        Ok(Self {
            kind: Kind::AllGroupsSummaryBuild {
                family,
                parameters,
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
    /// The `k` heaviest items of each input row's keyed state (a CMS or
    /// Count Sketch with heap), one output row per item. The state's heap
    /// capacity is its θ, which the state carries: a state whose heap holds
    /// fewer than `k` items is refused when it is read.
    pub fn keyed_evaluation(
        input: SchemaRef,
        state: usize,
        k: usize,
        output: SchemaRef,
    ) -> Result<Self, Error> {
        use planner_types::summary_type::SketchAlgorithm as A;
        let family = &field(&input, state)?.dtype;
        crate::values::validate_family(family)?;
        if !matches!(
            family,
            SummaryFamilyType::Sketch(A::CmsWithHeap | A::CountSketchWithHeap)
        ) {
            return Err(invalid("keyed evaluation requires a keyed summary state"));
        }
        if output.fields.len() <= input.fields.len() {
            return Err(invalid("invalid keyed evaluation shape"));
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
    /// over that column's values. `parameters` is the sketch's θ (`None`
    /// for an exact aggregate).
    pub fn per_group_summary_build(
        input: SchemaRef,
        family: SummaryFamilyType,
        parameters: Option<SketchParams>,
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
                SummaryFamilyType::ExactAggregate(kind, _) if kind.tracks_counter_increase()
            )
        {
            return Err(invalid("counter summary requires a timestamp column"));
        }
        crate::capability::validate_summary_kernel(
            &family,
            parameters.as_ref(),
            &SummaryUpdate::column(ColumnRef::SampleValue),
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
        });
        Ok(Self {
            kind: Kind::PerGroupSummaryBuild {
                family,
                parameters,
                value,
                time,
                groups,
                filter: None,
                emit_empty_global: false,
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
                emit_empty_global: false,
                parameters: None,
                seal_output: false,
            },
            inputs: vec![input],
            output: schema(fields),
        })
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
            SummaryFamilyType::ExactAggregate(planner_types::summary_type::ExactKind::Count, _)
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
                    planner_types::summary_type::ExactKind::Min
                        | planner_types::summary_type::ExactKind::Max,
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
    use planner_types::summary_type::{ExactKind, SketchAlgorithm as A};
    let SummaryFamilyType::Sketch(kind) = family else {
        return *dtype == DataType::Float64
            || matches!(
                family,
                SummaryFamilyType::ExactAggregate(ExactKind::Count, _)
            );
    };
    match dtype {
        DataType::Float64 | DataType::Int64 => true,
        DataType::Utf8 => matches!(kind, A::Kmv | A::Hll | A::UnivMon),
        DataType::Bool => kind == &A::UnivMon,
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
                SummaryEvaluation::Sketch(planner_types::summary_type::SketchStatistic::Cardinality),
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
    matches!(family, SummaryFamilyType::Sketch(kind)
        if kind == &planner_types::summary_type::SketchAlgorithm::Cms)
        && matches!(
            query,
            SummaryEvaluation::Sketch(planner_types::summary_type::SketchStatistic::PointCount {
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
            parameters,
            value,
            time,
            groups,
            filter,
            emit_empty_global,
        } => Ok(futures::stream::once(async move {
            build_per_group_summary(
                input,
                PerGroupBuild {
                    family,
                    parameters: parameters.as_ref(),
                    value: *value,
                    time: *time,
                    groups,
                    filter: filter.as_deref(),
                    emit_empty_global: *emit_empty_global,
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
            parameters,
            items,
            row_weight,
            groups,
            filter,
        } => Ok(futures::stream::once(async move {
            build_per_group_keyed_summary(
                input,
                KeyedBuild {
                    family,
                    parameters: parameters.as_ref(),
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
            parameters,
            item,
            row_weight,
            groups,
            filter,
        } => Ok(futures::stream::once(async move {
            build_all_groups_summary(
                input,
                AllGroupsBuild {
                    family,
                    parameters: parameters.as_ref(),
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
            emit_empty_global,
            parameters,
            seal_output,
        } = &operator.kind
        else {
            unreachable!()
        };
        merge_summary(
            input,
            *state,
            groups,
            MergeOutput {
                empty_global: emit_empty_global.then_some(parameters.as_ref()),
                seal: *seal_output,
            },
            &input_schema,
            &output,
            &context,
        )
        .await
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
    /// θ (`None` for an exact family).
    parameters: Option<&'a SketchParams>,
    value: usize,
    time: Option<usize>,
    groups: &'a [usize],
    filter: Option<&'a Expression>,
    /// Whether an empty input still has the global group (SQL; not PromQL):
    /// the kind's `emit_empty_global`.
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
    let mut updater = StateColumnUpdater::new(build.family, build.parameters)?;
    if let Some(max_out_of_order_ms) = context.max_out_of_order_ms() {
        updater = updater.with_max_out_of_order_ms(max_out_of_order_ms);
    }
    updater = updater.with_sample_block_codec(context.sample_block_codec().clone())?;
    let mut work = Cooperative::new(context);
    let mut workspace = Workspace::new(context)?;
    // A global SQL aggregate has one row over no input rows; a PromQL
    // aggregation of an empty vector produces no sample.
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
                timestamp_array(batch.column(time))
                    .expect("summary time columns are timestamps")
                    .clone()
            });
            let time_at = |row: usize| times.as_ref().map_or(0, |times| times.value(row));
            let values = batch.column(build.value);
            if let Some(values) = float64_array(values) {
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
    /// θ.
    parameters: Option<&'a SketchParams>,
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
    let mut updater = StateColumnUpdater::new(build.family, build.parameters)?;
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
    /// θ: the Hydra grid's sizes.
    parameters: Option<&'a SketchParams>,
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
    let mut grid =
        crate::factory::create_hydra_cms(build.family, build.parameters).map_err(Error::Invalid)?;
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

/// What a `SummaryMerge` outputs beyond the merged states.
#[derive(Clone, Copy)]
struct MergeOutput<'a> {
    /// `Some` of the states' θ: one empty state when there is no input
    /// state, as a global SQL aggregate has one row over no input rows.
    empty_global: Option<Option<&'a SketchParams>>,
    /// Whether the merged states are a stored level's windows, sealed as a
    /// chain seals its own.
    seal: bool,
}

/// Merge the states of each group across all input rows, as `shape` says.
async fn merge_summary(
    mut input: Input<'_, Batch>,
    state_column: usize,
    groups: &[usize],
    shape: MergeOutput<'_>,
    input_schema: &SchemaRef,
    output: &SchemaRef,
    context: &RunContext,
) -> Result<Batch, Error> {
    use crate::summary_kernels::exact::ScalarState;
    use crate::values::StateStorage;
    let family = &input_schema.fields[state_column].dtype;
    // Every family but the counter merges in any order: fold the input
    // batch by batch, holding one batch at a time. Counter states join
    // consecutive windows pairwise, in time order, so they are collected
    // and ordered first (stored counter records are mapped, not copied).
    if SummaryTypeCapability::of(family).merge_inputs != Some(MergeInputRule::DisjointInTime) {
        return merge_summary_streaming(
            input,
            state_column,
            groups,
            shape,
            input_schema,
            output,
            context,
        )
        .await;
    }
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
    if let (true, Some(parameters)) = (entries.is_empty(), shape.empty_global) {
        table.add_empty_group()?;
        let mut updater = StateColumnUpdater::new(family, parameters)?;
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
    // Counter states (the only family this path merges) join consecutive
    // windows pairwise, which is exact only in time order: order the inputs
    // by their window start.
    entries.sort_by_cached_key(|&entry| {
        let (states, index) = state_of(entry);
        match states.storage() {
            StateStorage::Exact(records) => records.get(index).earliest_sample_ms(),
            _ => states
                .state(index)
                .as_any()
                .downcast_ref::<crate::summary_kernels::exact::ExactAccumulator>()
                .and_then(|state| state.earliest_sample_ms()),
        }
    });
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
                    None => records.get(index),
                    Some(existing) => existing
                        .merge(&records.get(index))
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
    let mut merger = ObjectMerger::new(table_memory_bytes, context)?;
    for &entry in entries {
        work.checkpoint().await?;
        let (states, index) = state_of(entry);
        // Counter states are records or objects, never mapped sketches: no
        // KLL accumulator is seeded here.
        merger.add(states, index, entry.2 as usize, group_count, &|| 0)?;
    }
    Ok(merger.finish())
}

/// The input pair of a merged Hydra grid, kept so their addresses stay
/// unique while they key the memo, and the merged grid.
type GridPair = (
    Arc<crate::summary_kernels::HydraCms>,
    Arc<crate::summary_kernels::HydraCms>,
    Arc<crate::summary_kernels::HydraCms>,
);

/// Kernel-object states merged per group, one input state at a time.
struct ObjectMerger {
    merged: Vec<Option<(Arc<dyn crate::AggregateCore>, bool)>>,
    memory: crate::runtime::Reservation,
    retained: usize,
    /// States shared by several groups (Hydra grids) are reserved once each.
    shared: BTreeMap<usize, usize>,
    grids: BTreeMap<(usize, usize), GridPair>,
}

impl ObjectMerger {
    /// A merger holding a group table of `table_memory_bytes`.
    fn new(table_memory_bytes: usize, context: &RunContext) -> Result<Self, Error> {
        Ok(Self {
            merged: Vec::new(),
            memory: context.reserve(table_memory_bytes)?,
            retained: table_memory_bytes,
            shared: BTreeMap::new(),
            grids: BTreeMap::new(),
        })
    }

    /// The group table grew to `table_memory_bytes`.
    fn table_grew(&mut self, old_bytes: usize, table_memory_bytes: usize) -> Result<(), Error> {
        self.retained = self
            .retained
            .saturating_sub(old_bytes)
            .checked_add(table_memory_bytes)
            .ok_or(Error::MemoryLimit)?;
        self.memory.resize(self.retained)
    }

    /// Merges state `index` of `states` into group `group` (of `group_count`
    /// so far). `seed` is the group's seed for a KLL accumulator built from
    /// a mapped state, so the merge is the same on every run.
    fn add(
        &mut self,
        states: &StateColumn,
        index: usize,
        group: usize,
        group_count: usize,
        seed: &dyn Fn() -> u64,
    ) -> Result<(), Error> {
        use crate::summary_kernels::HydraCmsGroup;
        use crate::values::StateStorage;
        if self.merged.len() < group_count {
            self.merged.resize(group_count, None);
        }
        // A mapped state is folded into the group's accumulator where it is
        // stored: the accumulator is the only allocation.
        if let StateStorage::Mapped(mapped) = states.storage() {
            let slot = &mut self.merged[group];
            let Some((existing, existing_held_outside_run)) = slot else {
                let accumulator = mapped.accumulator(index, seed());
                self.retained = self
                    .retained
                    .checked_add(accumulator.approx_memory_bytes() + 64)
                    .ok_or(Error::MemoryLimit)?;
                self.memory.resize(self.retained)?;
                *slot = Some((accumulator, false));
                return Ok(());
            };
            // An accumulator shared with an input (passed through from an
            // owned column) is copied once, then merged into in place.
            if *existing_held_outside_run || Arc::strong_count(existing) != 1 {
                let owned: Arc<dyn crate::AggregateCore> = Arc::from(existing.clone_boxed_core());
                if !*existing_held_outside_run {
                    self.retained = self.retained.saturating_sub(existing.approx_memory_bytes());
                }
                *existing = owned;
                *existing_held_outside_run = false;
                self.retained = self
                    .retained
                    .checked_add(existing.approx_memory_bytes())
                    .ok_or(Error::MemoryLimit)?;
            }
            let old_bytes = existing.approx_memory_bytes();
            // Reserved before merging: a merge grows the accumulator by at
            // most the merged state's size.
            self.memory.resize(
                self.retained
                    .checked_add(mapped.state_len(index))
                    .ok_or(Error::MemoryLimit)?,
            )?;
            let accumulator = Arc::get_mut(existing).expect("the accumulator is the merger's own");
            mapped.merge_into(index, accumulator)?;
            self.retained = self
                .retained
                .saturating_sub(old_bytes)
                .checked_add(existing.approx_memory_bytes())
                .ok_or(Error::MemoryLimit)?;
            return self.memory.resize(self.retained);
        }
        let state = states.state(index);
        let held_outside_run = states.state_held_outside_run(index);
        // A stored state, and the grid it shares, are held outside the run.
        if let Some((id, bytes)) = state.shared_state().filter(|_| !held_outside_run) {
            if self.shared.insert(id, bytes).is_none() {
                self.retained = self.retained.checked_add(bytes).ok_or(Error::MemoryLimit)?;
                self.memory.resize(self.retained)?;
            }
        }
        let slot = &mut self.merged[group];
        let Some((existing, existing_held_outside_run)) = slot else {
            // Passed through: its slot, and the state unless stored.
            let own_bytes = if held_outside_run {
                0
            } else {
                state.approx_memory_bytes()
            };
            self.retained = self
                .retained
                .checked_add(own_bytes + 64)
                .ok_or(Error::MemoryLimit)?;
            self.memory.resize(self.retained)?;
            *slot = Some((state, held_outside_run));
            return Ok(());
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
            let grid = match self.grids.entry(pair) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.get().2.clone(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let grid = Arc::new(
                        left.grid
                            .merge(&right.grid)
                            .map_err(|e| Error::Operator(e.to_string()))?,
                    );
                    let bytes = grid.approx_memory_bytes();
                    self.shared.insert(Arc::as_ptr(&grid) as usize, bytes);
                    self.retained = self.retained.checked_add(bytes).ok_or(Error::MemoryLimit)?;
                    self.memory.resize(self.retained)?;
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
            return Ok(());
        }
        // A stored state was charged only its slot.
        let old_bytes = if was_held_outside_run {
            0
        } else {
            existing.approx_memory_bytes()
        };
        // Reserve an estimate for the replacement while both input states remain live.
        self.memory.resize(
            self.retained
                .checked_add(old_bytes)
                .and_then(|n| n.checked_add(state.approx_memory_bytes()))
                .ok_or(Error::MemoryLimit)?,
        )?;
        *existing = Arc::from(
            existing
                .merge_with(state.as_ref())
                .map_err(|e| Error::Operator(e.to_string()))?,
        );
        self.retained = self
            .retained
            .checked_sub(old_bytes)
            .and_then(|n| n.checked_add(existing.approx_memory_bytes()))
            .ok_or(Error::MemoryLimit)?;
        self.memory.resize(self.retained)
    }

    /// Each group's merged state so far, `None` for a group it merged none.
    fn finish_partial(mut self) -> Vec<Option<(Arc<dyn crate::AggregateCore>, bool)>> {
        std::mem::take(&mut self.merged)
    }

    /// Each group's merged state, with whether something outside the run
    /// holds it.
    fn finish(self) -> Vec<(Arc<dyn crate::AggregateCore>, bool)> {
        self.merged
            .into_iter()
            .map(|state| state.expect("every group has a row"))
            .collect()
    }
}

/// [`merge_summary`] for a family that merges in any order: each input
/// batch is folded into the groups' states as it arrives and then dropped,
/// so the merge holds one input batch at a time plus its accumulators.
async fn merge_summary_streaming(
    mut input: Input<'_, Batch>,
    state_column: usize,
    groups: &[usize],
    shape: MergeOutput<'_>,
    input_schema: &SchemaRef,
    output: &SchemaRef,
    context: &RunContext,
) -> Result<Batch, Error> {
    use crate::summary_kernels::exact::{ExactAccumulator, ScalarState};
    use crate::values::StateStorage;
    let family = &input_schema.fields[state_column].dtype;
    let exact_family = matches!(family, SummaryFamilyType::ExactAggregate(..));
    let mut table = GroupTable::new(groups.to_vec());
    let mut work = Cooperative::new(context);
    // Exact records merge in place, one record per group; a group with a
    // keyed exact state (an object, not a record) merges as objects.
    let mut exact: Vec<Option<ScalarState>> = Vec::new();
    let mut as_objects: Vec<bool> = Vec::new();
    let mut workspace = Workspace::new(context)?;
    let mut objects = ObjectMerger::new(0, context)?;
    let mut table_bytes = 0;
    while let Some(batch) = input.next().await {
        let batch = batch?;
        let _held = context.reserve(batch.memory_bytes())?;
        let ids = table.group_ids(&batch)?;
        let (states, indices) = state_indices(&batch, state_column)?;
        objects.table_grew(table_bytes, table.memory_bytes())?;
        table_bytes = table.memory_bytes();
        exact.resize(table.len(), None);
        as_objects.resize(table.len(), false);
        if exact_family {
            workspace.set_bytes(exact.len() * std::mem::size_of::<Option<ScalarState>>())?;
        }
        for (row, &group) in ids.iter().enumerate() {
            let (index, group) = (indices.value(row) as usize, group as usize);
            let seed = || group_seed(&table, group as u32, groups.len());
            if !exact_family {
                objects.add(states, index, group, table.len(), &seed)?;
                continue;
            }
            let record = match states.storage() {
                StateStorage::Exact(records) => Some(records.get(index)),
                _ => states
                    .state(index)
                    .as_any()
                    .downcast_ref::<ExactAccumulator>()
                    .and_then(ExactAccumulator::scalar)
                    .cloned(),
            };
            match (record, as_objects[group]) {
                (Some(record), false) => {
                    let slot = &mut exact[group];
                    *slot = Some(match slot.take() {
                        None => record,
                        Some(existing) => existing
                            .merge(&record)
                            .map_err(|e| Error::Operator(e.to_string()))?,
                    });
                }
                _ => {
                    // A keyed state: this group merges as objects from now
                    // on, starting from the records merged so far.
                    if !as_objects[group] {
                        as_objects[group] = true;
                        if let Some(record) = exact[group].take() {
                            let column = StateColumn::from_exact(family.clone(), vec![record]);
                            objects.add(&column, 0, group, table.len(), &seed)?;
                        }
                    }
                    objects.add(states, index, group, table.len(), &seed)?;
                }
            }
        }
        work.checkpoint_rows(batch.num_rows()).await?;
    }
    if table.len() == 0 {
        if let Some(parameters) = shape.empty_global {
            // A global SQL aggregate's one row over no input rows: one empty state.
            table.add_empty_group()?;
            let mut updater = StateColumnUpdater::new(family, parameters)?;
            updater.add_group()?;
            return grouped_states(&table, updater.seal(), input_schema, output);
        }
    }
    let states = if exact_family && !as_objects.contains(&true) {
        StateColumn::from_exact(
            family.clone(),
            exact
                .into_iter()
                .map(|record| record.expect("every group has a row"))
                .collect(),
        )
    } else if exact_family {
        // Some groups merged as objects: every group as an object.
        let mut merged = objects.finish_partial();
        merged.resize(exact.len(), None);
        for (group, record) in exact.into_iter().enumerate() {
            if let Some(record) = record {
                merged[group] = Some((
                    Arc::new(ExactAccumulator::from_scalar(family.clone(), record))
                        as Arc<dyn crate::AggregateCore>,
                    false,
                ));
            }
        }
        StateColumn::from_objects_of(
            family.clone(),
            merged
                .into_iter()
                .map(|state| state.expect("every group has a row"))
                .collect(),
        )
    } else if shape.seal {
        // A stored level's windows, sealed as a chain seals its own.
        let merged: Vec<_> = objects
            .finish()
            .into_iter()
            .map(|(state, _)| state)
            .collect();
        StateColumn::from_sealed_views(family.clone(), merged.iter().cloned())
            .unwrap_or_else(|| StateColumn::from_objects(family.clone(), merged))
    } else {
        StateColumn::from_objects_of(family.clone(), objects.finish())
    };
    grouped_states(&table, states, input_schema, output)
}

/// The seed of group `group`'s KLL accumulator: a hash of its key values,
/// so the same group merges the same way on every run.
fn group_seed(table: &GroupTable, group: u32, key_columns: usize) -> u64 {
    (0..key_columns).fold(0xcbf2_9ce4_8422_2325u64, |hash, column| {
        format!("{:?}", table.label(group, column))
            .bytes()
            .fold(hash, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            })
    })
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
            .evaluation(evaluation.counter_readout, range_ms)
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
        if !evaluation.statistic.tracks_counter_increase() {
            return false;
        }
        match states.storage() {
            StateStorage::Exact(records) => {
                evaluation.statistic == statistic_of(states.family())
                    && records.get(index).insufficient_counter_raw_data_samples()
            }
            StateStorage::Objects(objects) => {
                crate::evaluation::insufficient_counter_raw_data_samples(
                    objects[index].as_ref(),
                    evaluation.statistic,
                )
            }
            StateStorage::Mapped(_) => false,
            StateStorage::CompressedSamples(_) => false,
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
                exact_value(&records.get(index), evaluation)?
            }
            (SummaryEvaluation::Exact(evaluation), _) => {
                let state = states.state(index);
                let exact = state
                    .as_any()
                    .downcast_ref::<ExactAccumulator>()
                    .ok_or_else(|| invalid("exact evaluation requires exact state"))?;
                let record = exact
                    .scalar()
                    .ok_or_else(|| invalid("exact evaluation requires an unkeyed state"))?;
                exact_value(record, evaluation)?
            }
            // A mapped DDSketch's quantile, read on its view: no owned
            // sketch is built.
            (
                SummaryEvaluation::Sketch(planner_types::summary_type::SketchStatistic::Quantile {
                    q,
                }),
                StateStorage::Mapped(mapped),
            ) if !integral && mapped.kernel() == crate::values::MappedSketchKernel::DDSketch => {
                match mapped
                    .ddsketch_quantile(index, *q)
                    .map_err(Error::Operator)?
                {
                    Some(value) => Some(Value::Float64(value)),
                    // A filtered or NULL-only group's quantile is SQL NULL.
                    None if nullable => None,
                    None => {
                        return Err(Error::Operator(
                            "DDSketch quantile of an empty population".into(),
                        ))
                    }
                }
            }
            (SummaryEvaluation::Sketch(query), storage) => {
                let summary = match storage {
                    StateStorage::Objects(objects) => objects[index].clone(),
                    StateStorage::Exact(_)
                    | StateStorage::Mapped(_)
                    | StateStorage::CompressedSamples(_) => states.state(index),
                };
                // A filtered or NULL-only group's quantile is SQL NULL.
                if nullable && summary.is_empty() {
                    None
                } else {
                    let value = summary
                        .estimate(query)
                        .map_err(|e| Error::Operator(e.to_string()))?;
                    if integral
                        && *query == planner_types::summary_type::SketchStatistic::Cardinality
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
        // The heap capacity is the state's θ.
        let (_, _, capacity) = summary.shape();
        if k > capacity {
            return Err(Error::Invalid(format!(
                "top-{k} exceeds the state's heap capacity {capacity}"
            )));
        }
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
