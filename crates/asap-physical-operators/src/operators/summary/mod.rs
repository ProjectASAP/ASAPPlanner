use super::*;
/// A summary readout: a sketch query, or an exact readout with typed parameters.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ReadoutQuery {
    Sketch(planner_types::post_asap::SketchQuery),
    Exact(crate::summary_kernels::exact::ExactReadout),
}

impl Operator {
    pub fn keyed_summary_build(
        input: Schema,
        family: FieldDataType,
        value: usize,
        items: Vec<usize>,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        use crate::summary_kernels::weighted_frequency::WeightedFrequency;
        crate::values::validate_family(&family)?;
        let FieldDataType::Sketch(kind, _) = &family else {
            return Err(invalid("keyed sketch required"));
        };
        WeightedFrequency::configuration(kind)?;
        validate_groups(&input, &groups)?;
        if items.is_empty() || plain(&input, value)? != (&DataType::Float64, false) {
            return Err(invalid(
                "keyed summary requires identities and non-null Float64 weights",
            ));
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
        fields.push(Field {
            table: None,
            name: "state".into(),
            dtype: family.clone(),
            nullable: false,
        });
        Ok(Self {
            kind: Kind::KeyedSummaryBuild {
                family,
                value,
                items,
                groups,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    pub fn keyed_readout(
        input: Schema,
        state: usize,
        k: usize,
        output: Schema,
    ) -> Result<Self, Error> {
        use crate::summary_kernels::weighted_frequency::WeightedFrequency;
        crate::values::validate_family(&field(&input, state)?.dtype)?;
        let FieldDataType::Sketch(kind, _) = &field(&input, state)?.dtype else {
            return Err(invalid("keyed readout requires summary state"));
        };
        let (_, _, _, capacity) = WeightedFrequency::configuration(kind)?;
        if k > capacity || output.fields.len() <= input.fields.len() {
            return Err(invalid("invalid keyed readout shape or capacity"));
        }
        if state + 1 != input.fields.len()
            || output.fields[..state] != input.fields[..state]
            || output.fields.last().unwrap().dtype != FieldDataType::Plain(DataType::Float64)
        {
            return Err(invalid(
                "keyed readout must preserve partitions and return a Float64 score",
            ));
        }
        crate::values::validate_schema(&output)?;
        Ok(Self {
            kind: Kind::KeyedReadout { state, k },
            inputs: vec![input],
            output,
        })
    }
    pub fn summary_build(
        input: Schema,
        family: FieldDataType,
        value: usize,
        time: Option<usize>,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        crate::values::validate_family(&family)?;
        validate_groups(&input, &groups)?;
        if plain(&input, value)?.0 != &DataType::Float64 {
            return Err(invalid("summary numeric update requires Float64"));
        }
        if let Some(time) = time {
            if plain(&input, time)? != (&DataType::Timestamp, false) {
                return Err(invalid("summary time column must be a timestamp"));
            }
        }
        if time.is_none()
            && matches!(
                family,
                FieldDataType::ExactAggregate(
                    planner_types::post_asap::ExactKind::Rate
                        | planner_types::post_asap::ExactKind::Increase,
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
        fields.push(Field {
            table: None,
            name: "state".into(),
            dtype: family.clone(),
            nullable: false,
        });
        Ok(Self {
            kind: Kind::SummaryBuild {
                family,
                value,
                time,
                groups,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    pub fn summary_merge(input: Schema, state: usize, groups: Vec<usize>) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        crate::values::validate_family(&field(&input, state)?.dtype)?;
        if matches!(field(&input, state)?.dtype, FieldDataType::Plain(_)) {
            return Err(invalid("summary state required"));
        }
        let mut fields = groups
            .iter()
            .map(|&i| input.fields[i].clone())
            .collect::<Vec<_>>();
        fields.push(input.fields[state].clone());
        Ok(Self {
            kind: Kind::SummaryMerge { state, groups },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    pub fn readout(input: Schema, state: usize, query: ReadoutQuery) -> Result<Self, Error> {
        let family = &field(&input, state)?.dtype;
        crate::values::validate_family(family)?;
        match &query {
            ReadoutQuery::Sketch(query) => {
                crate::capability::validate_sketch_readout(family, query)?
            }
            ReadoutQuery::Exact(readout) => {
                crate::capability::validate_exact_readout(family, readout)?
            }
        }
        let mut fields = input.fields.clone();
        let result_type = if matches!(
            fields[state].dtype,
            FieldDataType::ExactAggregate(planner_types::post_asap::ExactKind::Count, _)
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
                FieldDataType::ExactAggregate(
                    planner_types::post_asap::ExactKind::Min
                        | planner_types::post_asap::ExactKind::Max,
                    _
                )
            );
        fields[state] = result_field("value", result_type, nullable);
        Ok(Self {
            kind: Kind::Readout { state, query },
            inputs: vec![input],
            output: schema(fields),
        })
    }
}
/// The Planner reads a Count-Min bare count only for count intents, whose
/// output is Int64 and whose updates have unit weight; execution rejects a
/// non-integral total rather than rounding it.
fn integral_count(family: &FieldDataType, query: &ReadoutQuery) -> bool {
    matches!(family, FieldDataType::Sketch(kind, _)
        if kind.algorithm() == &planner_types::post_asap::SketchAlgorithm::Cms)
        && matches!(
            query,
            ReadoutQuery::Sketch(planner_types::post_asap::SketchQuery::PointCount {
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
    let range_ms = operator.readout_range(&context)?;
    let output = operator.output.clone();
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    match &operator.kind {
        Kind::SummaryBuild {
            family,
            value,
            time,
            groups,
        } => Ok(futures::stream::once(async move {
            Batch::try_new(
                output,
                build_summary(input, family, *value, *time, groups, &context).await?,
            )
        })
        .boxed_local()),
        Kind::KeyedSummaryBuild {
            family,
            value,
            items,
            groups,
        } => Ok(futures::stream::once(async move {
            Batch::try_new(
                output,
                build_keyed_summary(input, family, *value, items, groups, &context).await?,
            )
        })
        .boxed_local()),
        Kind::KeyedReadout { state, k } => Ok(input
            .map(move |batch| {
                let batch = batch?;
                let mut rows = Vec::new();
                for row in batch.rows() {
                    let Value::Summary { state: summary, .. } = &row[*state] else {
                        return Err(invalid("summary value required"));
                    };
                    let summary = summary
                        .as_any()
                        .downcast_ref::<crate::summary_kernels::weighted_frequency::WeightedFrequency>()
                        .ok_or_else(|| invalid("weighted frequency typed state required"))?;
                    for items in summary.rows(*k) {
                        let mut values = row[..*state].to_vec();
                        values.extend(items);
                        // The typed output schema restores epoch-millisecond
                        // timestamp keys from the kernel's Int64 representation.
                        for (value, field) in values.iter_mut().zip(&output.fields) {
                            if field.dtype == FieldDataType::Plain(DataType::Timestamp) {
                                if let Value::Int64(time) = value {
                                    *value = Value::Timestamp(*time);
                                }
                            }
                        }
                        rows.push(values);
                    }
                }
                Batch::try_new(output.clone(), rows)
            })
            .boxed_local()),
        Kind::Readout { state, query } => Ok(input
            .map(move |batch| {
                let batch = batch?;
                let mut rows = batch.rows().to_vec();
                if let ReadoutQuery::Exact(readout) = query {
                    rows.retain(|row| !matches!(&row[*state], Value::Summary { state: summary, .. }
                        if crate::readout::insufficient_counter_samples(summary.as_ref(), readout.statistic)));
                }
                for row in &mut rows {
                    let Value::Summary { state: summary, .. } = &row[*state] else {
                        return Err(invalid("summary value required"));
                    };
                    row[*state] = match query {
                        ReadoutQuery::Sketch(query) => {
                            let value = summary
                                .estimate(query)
                                .map_err(|e| Error::Operator(e.to_string()))?;
                            if output.fields[*state].dtype
                                == FieldDataType::Plain(DataType::Int64)
                            {
                                // Below 2^53 an f64 sum of unit updates is exact.
                                if value.fract() != 0.0 || !(0.0..9.007_199_254_740_992e15).contains(&value) {
                                    return Err(Error::Operator(
                                        "Count-Min count is not an exact integer".into(),
                                    ));
                                }
                                Value::Int64(value as i64)
                            } else {
                                Value::Float64(value)
                            }
                        }
                        ReadoutQuery::Exact(readout) => {
                            let exact = summary
                                .as_any()
                                .downcast_ref::<crate::summary_kernels::exact::ExactAccumulator>()
                                .ok_or_else(|| invalid("exact readout requires exact state"))?;
                            if output.fields[*state].dtype == FieldDataType::Plain(DataType::Int64) {
                                let count = exact.count().ok_or_else(|| {
                                    Error::Operator("exact count state lacks an integer count".into())
                                })?;
                                Value::Int64(i64::try_from(count).map_err(|_| {
                                    Error::Operator("exact count exceeds Int64".into())
                                })?)
                            } else {
                                match exact
                                    .readout(readout.statistic, range_ms, None)
                                    .map_err(|e| Error::Operator(e.to_string()))?
                                {
                                    Some(value) => Value::Float64(value),
                                    None if output.fields[*state].nullable => Value::Null,
                                    None => {
                                        return Err(Error::Operator(
                                            "empty exact population".into(),
                                        ))
                                    }
                                }
                            }
                        }
                    };
                }
                Batch::try_new(output.clone(), rows)
            })
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
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    Ok(futures::stream::once(async move {
        let (rows, _memory) = collect_rows(input, &context).await?;
        let result = match &operator.kind {
            Kind::SummaryMerge { state, groups } => {
                merge_summary(rows, *state, groups, &context).await?
            }
            _ => unreachable!(),
        };
        Batch::try_new(output, result)
    })
    .boxed_local())
}

async fn build_summary(
    mut input: Input<'_, Batch>,
    family: &FieldDataType,
    value: usize,
    time: Option<usize>,
    groups: &[usize],
    context: &RunContext,
) -> Result<Vec<Vec<Value>>, Error> {
    type State = (
        Vec<Value>,
        Box<dyn crate::factory::AccumulatorUpdater>,
        Reservation,
        usize,
        Option<i64>,
    );
    let create = |labels: Vec<Value>, key_bytes: usize| -> Result<State, Error> {
        let updater = crate::factory::create_planner_accumulator(
            family,
            &SummaryUpdate::column(ColumnRef::SampleValue),
            &Default::default(),
        )
        .map_err(Error::Operator)?;
        let overhead = labels.iter().map(Value::bytes).sum::<usize>() + key_bytes + 64;
        let memory = context.reserve(updater.memory_usage_bytes() + overhead)?;
        Ok((labels, updater, memory, overhead, None))
    };
    let mut work = Cooperative::new(context);
    let mut states = BTreeMap::<Vec<Vec<u8>>, State>::new();
    if groups.is_empty() {
        states.insert(vec![], create(vec![], 0)?);
    }
    let ordered_time = matches!(
        family,
        FieldDataType::ExactAggregate(
            planner_types::post_asap::ExactKind::Rate
                | planner_types::post_asap::ExactKind::Increase,
            _
        )
    );
    while let Some(batch) = input.next().await {
        let batch = batch?;
        for row in batch.rows() {
            work.checkpoint().await?;
            let key = group_key(row, groups)?;
            if !states.contains_key(&key) {
                let labels = groups.iter().map(|&i| row[i].clone()).collect();
                let state = create(
                    labels,
                    key.iter()
                        .map(|v| v.len() + std::mem::size_of::<Vec<u8>>())
                        .sum(),
                )?;
                states.insert(key.clone(), state);
            }
            let (_, updater, memory, overhead, previous) =
                states.get_mut(&key).expect("inserted group");
            // SQL aggregates ignore NULL samples while retaining the group.
            // A missing counter sample also contributes no observation.
            let value = match row[value] {
                Value::Float64(value) => value,
                Value::Null => continue,
                _ => return Err(invalid("summary update type")),
            };
            let timestamp = if let Some(time) = time {
                let Value::Timestamp(time) = row[time] else {
                    return Err(invalid("summary time type"));
                };
                time
            } else {
                0
            };
            if ordered_time && previous.is_some_and(|prior| timestamp <= prior) {
                return Err(Error::Operator(
                    "counter samples must have strictly increasing timestamps within each group"
                        .into(),
                ));
            }
            updater
                .validate_single_input(value)
                .map_err(Error::Operator)?;
            updater.update_single(value, timestamp);
            *previous = Some(timestamp);
            memory.resize(updater.memory_usage_bytes() + *overhead)?;
        }
    }
    Ok(states
        .into_values()
        .map(|(mut labels, updater, _memory, _, _)| {
            labels.push(Value::Summary {
                family: family.clone(),
                state: Arc::from(updater.into_accumulator()),
            });
            labels
        })
        .collect())
}
async fn merge_summary(
    rows: Vec<Vec<Value>>,
    state_column: usize,
    groups: &[usize],
    context: &RunContext,
) -> Result<Vec<Vec<Value>>, Error> {
    type GroupState = (Vec<Value>, FieldDataType, Arc<dyn crate::AggregateCore>);
    let mut states: BTreeMap<Vec<Vec<u8>>, GroupState> = BTreeMap::new();
    let mut work = Cooperative::new(context);
    let mut memory = context.reserve(0)?;
    let mut retained = 0usize;
    for row in rows {
        work.checkpoint().await?;
        let Value::Summary { family, state } = &row[state_column] else {
            return Err(invalid("summary state required"));
        };
        let key = group_key(&row, groups)?;
        if let Some((_, expected, existing)) = states.get_mut(&key) {
            if expected != family {
                return Err(invalid("incompatible summary family"));
            }
            let old_bytes = existing.approx_memory_bytes();
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
        } else {
            retained = retained
                .checked_add(key_bytes(&key) + row_bytes(&row))
                .ok_or(Error::MemoryLimit)?;
            memory.resize(retained)?;
            states.insert(
                key,
                (
                    groups.iter().map(|&i| row[i].clone()).collect(),
                    family.clone(),
                    state.clone(),
                ),
            );
        }
    }
    Ok(states
        .into_values()
        .map(|(mut keys, family, state)| {
            keys.push(Value::Summary { family, state });
            keys
        })
        .collect())
}

async fn build_keyed_summary(
    mut input: Input<'_, Batch>,
    family: &FieldDataType,
    value: usize,
    items: &[usize],
    groups: &[usize],
    context: &RunContext,
) -> Result<Vec<Vec<Value>>, Error> {
    use crate::{summary_kernels::weighted_frequency::WeightedFrequency, AggregateCore};
    let FieldDataType::Sketch(kind, _) = family else {
        unreachable!()
    };
    let (algorithm, width, depth, capacity) = WeightedFrequency::configuration(kind)?;
    let mut work = Cooperative::new(context);
    let mut states =
        BTreeMap::<Vec<Vec<u8>>, (Vec<Value>, WeightedFrequency, Reservation, usize)>::new();
    while let Some(batch) = input.next().await {
        let batch = batch?;
        for row in batch.rows() {
            work.checkpoint().await?;
            let key = group_key(row, groups)?;
            if !states.contains_key(&key) {
                let labels = groups.iter().map(|&i| row[i].clone()).collect::<Vec<_>>();
                let overhead = labels.iter().map(Value::bytes).sum::<usize>()
                    + key.iter().map(|v| v.len() + 24).sum::<usize>()
                    + 128;
                let bytes = width
                    .checked_mul(depth)
                    .and_then(|n| n.checked_mul(8))
                    .and_then(|n| n.checked_add(overhead))
                    .ok_or_else(|| invalid("weighted frequency memory size overflow"))?;
                let reservation = context.reserve(bytes)?;
                states.insert(
                    key.clone(),
                    (
                        labels,
                        WeightedFrequency::new(algorithm, width, depth, capacity)?,
                        reservation,
                        overhead,
                    ),
                );
            }
            let (_, summary, reservation, overhead) = states.get_mut(&key).unwrap();
            let Value::Float64(weight) = row[value] else {
                return Err(invalid("weighted frequency weight type"));
            };
            summary.update(
                &items
                    .iter()
                    .map(|&i| match &row[i] {
                        Value::Timestamp(time) => Value::Int64(*time),
                        value => value.clone(),
                    })
                    .collect::<Vec<_>>(),
                weight,
            )?;
            reservation.resize(summary.approx_memory_bytes() + *overhead)?;
        }
    }
    Ok(states
        .into_values()
        .map(|(mut labels, summary, _, _)| {
            labels.push(Value::Summary {
                family: family.clone(),
                state: Arc::new(summary),
            });
            labels
        })
        .collect())
}
