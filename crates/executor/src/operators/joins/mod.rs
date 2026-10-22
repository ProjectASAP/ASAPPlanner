use super::*;
impl Operator {
    pub fn semi_join(
        left: SchemaRef,
        right: SchemaRef,
        keys: Vec<(usize, usize)>,
    ) -> Result<Self, Error> {
        if keys.is_empty() {
            return Err(invalid("semi-join needs matching keys"));
        }
        for &(l, r) in &keys {
            if plain(&left, l)?.0 != plain(&right, r)?.0 {
                return Err(invalid("join key types differ"));
            }
        }
        Ok(Self {
            kind: Kind::SemiJoin {
                keys,
                require_complete_right: false,
            },
            inputs: vec![left.clone(), right],
            output: left,
        })
    }
    /// Require every candidate key to have an authoritative value at execution.
    pub fn certified_semi_join(
        left: SchemaRef,
        right: SchemaRef,
        keys: Vec<(usize, usize)>,
    ) -> Result<Self, Error> {
        Ok(Self::semi_join(left, right, keys)?.require_complete_right())
    }

    pub(crate) fn require_complete_right(mut self) -> Self {
        if let Kind::SemiJoin {
            require_complete_right,
            ..
        } = &mut self.kind
        {
            *require_complete_right = true;
        }
        self
    }
    pub(crate) fn certified_pruning_keys(&self) -> Option<&[(usize, usize)]> {
        match &self.kind {
            Kind::SemiJoin {
                keys,
                require_complete_right: true,
            } => Some(keys),
            _ => None,
        }
    }
    pub fn relational_join(
        left: SchemaRef,
        right: SchemaRef,
        kind: planner_types::ir::operator::JoinKind,
        predicate: &planner_types::ir::Predicate,
        output: SchemaRef,
    ) -> Result<Self, Error> {
        use planner_types::ir::operator::JoinKind;
        let mut joined = left.fields.clone();
        joined.extend(right.fields.clone());
        let predicate =
            crate::expressions::CompiledExpression::compile(&predicate.0, &schema(joined.clone()))?;
        if predicate.dtype().0 != DataType::Bool {
            return Err(invalid("join predicate must be boolean"));
        }
        let fields = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            left.fields.clone()
        } else {
            for field in &mut joined[..left.fields.len()] {
                if matches!(kind, JoinKind::Right | JoinKind::Full) {
                    field.nullable = true;
                }
            }
            for field in &mut joined[left.fields.len()..] {
                if matches!(kind, JoinKind::Left | JoinKind::Full) {
                    field.nullable = true;
                }
            }
            joined
        };
        Self {
            kind: Kind::Join {
                kind,
                predicate: Box::new(predicate),
            },
            inputs: vec![left, right],
            output: schema(fields),
        }
        .with_output_schema(output)
    }
}
pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let output = operator.output.clone();
    if let Kind::Join { kind, predicate } = &operator.kind {
        let right = inputs.pop().ok_or_else(|| invalid("right input missing"))?;
        let left = inputs.pop().ok_or_else(|| invalid("left input missing"))?;
        return Ok(futures::stream::once(async move {
            let (left, right) = futures::try_join!(
                collect_batch(left, &operator.inputs[0], &context),
                collect_batch(right, &operator.inputs[1], &context)
            )?;
            relational_join(
                operator,
                kind,
                &predicate.at_evaluation_time(&context),
                &left.batch,
                &right.batch,
                &context,
            )
            .await
        })
        .boxed_local());
    }
    if let Kind::SemiJoin {
        keys,
        require_complete_right,
    } = &operator.kind
    {
        let right = inputs.pop().ok_or_else(|| invalid("right input missing"))?;
        let left = inputs.pop().ok_or_else(|| invalid("left input missing"))?;
        return Ok(futures::stream::once(async move {
            use super::aligned_binary::{key_batch, matchable_rows};
            // Poll both branches together: either may depend on a common producer.
            let (left, right) = futures::try_join!(
                collect_batch(left, &operator.inputs[0], &context),
                collect_batch(right, &operator.inputs[1], &context)
            )?;
            let (left, right) = (&left.batch, &right.batch);
            let left_keys = key_batch(left, &keys.iter().map(|(l, _)| *l).collect::<Vec<_>>())?;
            let right_keys = key_batch(right, &keys.iter().map(|(_, r)| *r).collect::<Vec<_>>())?;
            let mut table = super::group_table::GroupTable::new((0..keys.len()).collect());
            let mut work = Cooperative::new(&context);
            let mut workspace = Workspace::new(&context)?;
            // The right side's matchable keys.
            let mut members = ahash::AHashSet::new();
            let right_ids =
                chunked_group_ids(&mut table, &right_keys, &mut workspace, 0, &mut work).await?;
            for (id, matchable) in right_ids.into_iter().zip(matchable_rows(&right_keys)) {
                if matchable {
                    members.insert(id);
                } else if *require_complete_right {
                    return Err(invalid(
                        "certified pruning candidate has an unmatchable key",
                    ));
                }
            }
            work.checkpoint_rows(right.num_rows()).await?;
            workspace.set_bytes(table.memory_bytes() + 32 * members.len())?;
            let mut covered = ahash::AHashSet::new();
            let held = 32 * members.len();
            let left_ids =
                chunked_group_ids(&mut table, &left_keys, &mut workspace, held, &mut work).await?;
            let keep = left_ids
                .into_iter()
                .zip(matchable_rows(&left_keys))
                .map(|(id, matchable)| {
                    let kept = matchable && members.contains(&id);
                    if kept && *require_complete_right {
                        covered.insert(id);
                    }
                    Some(kept)
                })
                .collect::<arrow::array::BooleanArray>();
            work.checkpoint_rows(left.num_rows()).await?;
            if *require_complete_right && members != covered {
                return Err(invalid("certified pruning key has no authoritative value"));
            }
            let kept = left.filter(&keep)?;
            workspace.grow(kept.memory_bytes())?;
            let _ = output;
            Ok(kept)
        })
        .boxed_local());
    }
    unreachable!()
}

/// A nested-loop join: each left row against all right rows at once, its
/// predicate one column over the pairs.
async fn relational_join(
    operator: &Operator,
    kind: &planner_types::ir::operator::JoinKind,
    predicate: &crate::expressions::CompiledExpression,
    left: &Batch,
    right: &Batch,
    context: &RunContext,
) -> Result<Batch, Error> {
    use arrow::array::{Array, UInt32Array};
    use planner_types::ir::operator::JoinKind;
    let output = &operator.output;
    let mut workspace = Workspace::new(context)?;
    let mut work = Cooperative::new(context);
    workspace.grow(right.num_rows())?;
    let left_width = operator.inputs[0].fields.len();
    // The schema of a joined pair: left columns, then right columns.
    let mut joined_fields = operator.inputs[0].fields.clone();
    joined_fields.extend(operator.inputs[1].fields.iter().cloned());
    let joined_schema = schema(joined_fields);
    // The output pairs: a left row (or none), a right row (or none).
    let (mut out_left, mut out_right): (Vec<Option<u32>>, Vec<Option<u32>>) =
        (Vec::new(), Vec::new());
    let mut right_matched = vec![false; right.num_rows()];
    for left_row in 0..left.num_rows() as u32 {
        // The pairs of this left row with every right row.
        let matches: Vec<bool> = if *kind == JoinKind::Cross {
            vec![true; right.num_rows()]
        } else {
            let pairs = joined(left, right, left_row, &joined_schema)?;
            match predicate.evaluate_batch(&pairs) {
                Ok(result) => {
                    let result = result
                        .as_any()
                        .downcast_ref::<arrow::array::BooleanArray>()
                        .ok_or_else(|| invalid("join predicate must be boolean"))?
                        .clone();
                    (0..right.num_rows())
                        .map(|row| result.is_valid(row) && result.value(row))
                        .collect()
                }
                // A failing pair fails only if the row join reaches it:
                // replay this left row pair by pair, as the row join did.
                Err(_) => {
                    let rows = pairs.to_rows();
                    let mut matches = Vec::with_capacity(rows.len());
                    for row in rows {
                        let matched = matches!(predicate.evaluate(&row)?, Value::Bool(true));
                        matches.push(matched);
                        if matched && matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                            break;
                        }
                    }
                    matches.resize(right.num_rows(), false);
                    matches
                }
            }
        };
        work.checkpoint_rows(right.num_rows().max(1)).await?;
        let mut matched = false;
        for (right_row, &is_match) in matches.iter().enumerate() {
            if !is_match {
                continue;
            }
            matched = true;
            right_matched[right_row] = true;
            match kind {
                JoinKind::Semi => {
                    out_left.push(Some(left_row));
                    break;
                }
                JoinKind::Anti => break,
                _ => {
                    out_left.push(Some(left_row));
                    out_right.push(Some(right_row as u32));
                }
            }
        }
        if !matched {
            match kind {
                JoinKind::Left | JoinKind::Full => {
                    out_left.push(Some(left_row));
                    out_right.push(None);
                }
                JoinKind::Anti => out_left.push(Some(left_row)),
                _ => {}
            }
        }
        workspace.set_bytes(right.num_rows() + 8 * (out_left.len() + out_right.len()))?;
    }
    if matches!(kind, JoinKind::Right | JoinKind::Full) {
        for (right_row, matched) in right_matched.into_iter().enumerate() {
            if !matched {
                out_left.push(None);
                out_right.push(Some(right_row as u32));
            }
        }
    }
    let rows = out_left.len();
    // The output columns: the left rows' cells, then the right rows' cells,
    // NULL for a missing side (the output declares those columns nullable).
    let take =
        |batch: &Batch, indices: Vec<Option<u32>>| -> Result<Vec<arrow::array::ArrayRef>, Error> {
            let indices = UInt32Array::from(indices);
            if indices.null_count() > 0
                && (0..batch.schema().fields.len()).any(|c| batch.state_column(c).is_some())
            {
                return Err(invalid("a summary column has no NULL row"));
            }
            batch
                .record_batch()
                .columns()
                .iter()
                .map(|array| {
                    arrow::compute::take(array.as_ref(), &indices, None)
                        .map_err(|e| invalid(&e.to_string()))
                })
                .collect()
        };
    let mut arrays = take(left, out_left)?;
    let mut states = (0..left_width)
        .map(|c| left.state_column(c).cloned())
        .collect::<Vec<_>>();
    if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
        arrays.extend(take(right, out_right)?);
        states.extend((0..right.schema().fields.len()).map(|c| right.state_column(c).cloned()));
    }
    let result = Batch::from_parts(output.clone(), arrays, states, rows)?;
    workspace.grow(result.memory_bytes())?;
    Ok(result)
}

/// Left row `left_row` beside every right row: the pairs a join predicate
/// is evaluated on. The right columns are shared; only the left row is
/// repeated.
fn joined(
    left: &Batch,
    right: &Batch,
    left_row: u32,
    joined_schema: &SchemaRef,
) -> Result<Batch, Error> {
    let rows = right.num_rows();
    let repeated = left.take(&arrow::array::UInt32Array::from(vec![left_row; rows]))?;
    let mut arrays = repeated.record_batch().columns().to_vec();
    arrays.extend(right.record_batch().columns().iter().cloned());
    let mut states = (0..repeated.schema().fields.len())
        .map(|c| repeated.state_column(c).cloned())
        .collect::<Vec<_>>();
    states.extend((0..right.schema().fields.len()).map(|c| right.state_column(c).cloned()));
    Batch::from_parts(joined_schema.clone(), arrays, states, rows)
}
