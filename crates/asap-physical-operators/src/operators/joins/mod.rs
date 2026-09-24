use super::*;
impl Operator {
    pub fn semi_join(
        left: Schema,
        right: Schema,
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
            kind: Kind::SemiJoin { keys },
            inputs: vec![left.clone(), right],
            output: left,
        })
    }
    pub fn relational_join(
        left: Schema,
        right: Schema,
        kind: planner_types::pre_asap::JoinKind,
        predicate: &planner_types::pre_asap::Predicate,
        output: Schema,
    ) -> Result<Self, Error> {
        use planner_types::pre_asap::JoinKind;
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
            use planner_types::pre_asap::JoinKind;
            let ((left, _left_memory), (right, _right_memory)) =
                futures::try_join!(collect_rows(left, &context), collect_rows(right, &context))?;
            let mut workspace = Workspace::new(&context)?;
            let mut work = Cooperative::new(&context);
            workspace.grow(right.len())?;
            let mut result = Vec::new();
            let mut right_matched = vec![false; right.len()];
            for left_row in &left {
                work.checkpoint().await?;
                let mut matched = false;
                for (i, right_row) in right.iter().enumerate() {
                    work.checkpoint().await?;
                    let mut joined = left_row.clone();
                    joined.extend(right_row.iter().cloned());
                    if *kind == JoinKind::Cross
                        || matches!(predicate.evaluate(&joined)?, Value::Bool(true))
                    {
                        matched = true;
                        right_matched[i] = true;
                        match kind {
                            JoinKind::Semi => {
                                workspace.grow(row_bytes(left_row))?;
                                result.push(left_row.clone());
                                break;
                            }
                            JoinKind::Anti => break,
                            _ => {
                                workspace.grow(row_bytes(&joined))?;
                                result.push(joined);
                            }
                        }
                    }
                }
                if !matched {
                    match kind {
                        JoinKind::Left | JoinKind::Full => {
                            let mut joined = left_row.clone();
                            joined.resize(
                                joined.len() + operator.inputs[1].fields.len(),
                                Value::Null,
                            );
                            workspace.grow(row_bytes(&joined))?;
                            result.push(joined);
                        }
                        JoinKind::Anti => {
                            workspace.grow(row_bytes(left_row))?;
                            result.push(left_row.clone());
                        }
                        _ => {}
                    }
                }
            }
            if matches!(kind, JoinKind::Right | JoinKind::Full) {
                for (matched, row) in right_matched.into_iter().zip(right) {
                    work.checkpoint().await?;
                    if !matched {
                        let mut joined = vec![Value::Null; operator.inputs[0].fields.len()];
                        joined.extend(row);
                        workspace.grow(row_bytes(&joined))?;
                        result.push(joined);
                    }
                }
            }
            Batch::try_new(output, result)
        })
        .boxed_local());
    }
    if let Kind::SemiJoin { keys } = &operator.kind {
        let right = inputs.pop().ok_or_else(|| invalid("right input missing"))?;
        let left = inputs.pop().ok_or_else(|| invalid("left input missing"))?;
        return Ok(futures::stream::once(async move {
            // Poll both branches together: either may depend on a common producer.
            let ((left, _left_memory), (right, _right_memory)) =
                futures::try_join!(collect_rows(left, &context), collect_rows(right, &context))?;
            let right_cols = keys.iter().map(|(_, r)| *r).collect::<Vec<_>>();
            let left_cols = keys.iter().map(|(l, _)| *l).collect::<Vec<_>>();
            let mut members = std::collections::BTreeSet::new();
            let mut workspace = Workspace::new(&context)?;
            let mut work = Cooperative::new(&context);
            for row in &right {
                work.checkpoint().await?;
                if right_cols.iter().all(|&i| matchable_key(&row[i])) {
                    let key = group_key(row, &right_cols)?;
                    if !members.contains(&key) {
                        workspace.grow(key_bytes(&key))?;
                        members.insert(key);
                    }
                }
            }
            let mut rows = Vec::new();
            for row in left {
                work.checkpoint().await?;
                if left_cols.iter().all(|&i| matchable_key(&row[i]))
                    && members.contains(&group_key(&row, &left_cols)?)
                {
                    workspace.grow(std::mem::size_of::<Vec<Value>>())?;
                    rows.push(row);
                }
            }
            Batch::try_new(output, rows)
        })
        .boxed_local());
    }
    unreachable!()
}

// Group keys canonicalize NaNs, but equality joins must not match them.
fn matchable_key(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Float64(v) => !v.is_nan(),
        _ => true,
    }
}
