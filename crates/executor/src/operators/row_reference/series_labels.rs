//! The row-at-a-time PromQL series operators (reference for tests).
use super::super::series_labels::{
    apply, label_bytes, layout, parse_bound, signature, Labels, Layout,
};
use super::super::*;
use crate::expressions::binary::{BinaryOpKind, BinaryOperator};
use planner_types::ir::operator::VectorMatchKind;

fn float(layout: &Layout, row: &[Value]) -> Result<f64, Error> {
    match row[layout.value] {
        Value::Float64(value) => Ok(value),
        _ => Err(invalid("vector value must be Float64")),
    }
}

/// The value of a matched pair, or `None` when a comparison filters it out.
/// A filter keeps the left value.
async fn series_binary(
    operator: &Operator,
    binary: &BinaryOperator,
    scalars: [bool; 2],
    rows: [Vec<Vec<Value>>; 2],
    work: &mut Cooperative,
    workspace: &mut Workspace,
) -> Result<Vec<Vec<Value>>, Error> {
    use planner_types::ir::operator::{GroupSide, PromQLVectorSetOpKind};
    let drops_name = matches!(
        binary.kind,
        BinaryOpKind::Arithmetic(_) | BinaryOpKind::CompareBool(_)
    );
    let scalar = |rows: &[Vec<Value>]| match rows {
        [row] => match row[0] {
            Value::Float64(value) => Ok(value),
            _ => Err(invalid("PromQL scalar must be Float64")),
        },
        _ => Err(invalid("PromQL scalar operand must have one row")),
    };
    let [left, right] = rows;
    let mut result = Vec::new();
    if scalars == [true, true] {
        let value = apply(binary, scalar(&left)?, scalar(&right)?)?
            .ok_or_else(|| invalid("scalar comparison requires bool"))?;
        return Ok(vec![vec![Value::Float64(value)]]);
    }
    if scalars[0] || scalars[1] {
        let (constant, vector, schema) = if scalars[0] {
            (scalar(&left)?, right, &operator.inputs[1])
        } else {
            (scalar(&right)?, left, &operator.inputs[0])
        };
        let layout = layout(schema)?;
        let mut seen = std::collections::BTreeSet::new();
        for mut row in vector {
            work.checkpoint().await?;
            let value = float(&layout, &row)?;
            let (l, r) = if scalars[0] {
                (constant, value)
            } else {
                (value, constant)
            };
            let Some(mut computed) = apply(binary, l, r)? else {
                continue;
            };
            // A filter keeps the vector's value, even on the right.
            if matches!(binary.kind, BinaryOpKind::Compare(_)) {
                computed = value;
            }
            let mut set = layout.read(schema, &row)?;
            if drops_name {
                set.remove("__name__");
                layout.write(schema, &mut row, &set)?;
            }
            row[layout.value] = Value::Float64(computed);
            workspace.grow(row_bytes(&row) + label_bytes(&set))?;
            if !seen.insert(set) {
                return Err(invalid(
                    "vector cannot contain metrics with the same labelset",
                ));
            }
            result.push(row);
        }
        return Ok(result);
    }
    let schemas = [&operator.inputs[0], &operator.inputs[1]];
    let layouts = [layout(schemas[0])?, layout(schemas[1])?];
    let (kind, names, grouping) = match &binary.vector_match {
        None => (VectorMatchKind::Ignoring, &[][..], None),
        Some(m) => (m.kind.clone(), m.labels.as_slice(), m.grouping.as_ref()),
    };
    let read = |side: usize, row: &[Value]| layouts[side].read(schemas[side], row);
    let output = &operator.output;
    let output_layout = layout(output)?;
    let render =
        |side: usize, row: &[Value], labels: &Labels, value: f64| -> Result<Vec<Value>, Error> {
            let mut result = vec![Value::Null; output.fields.len()];
            if let (Some(to), Some(from)) = (output.time_index, schemas[side].time_index) {
                result[to] = row[from].clone();
            }
            output_layout.write(output, &mut result, labels)?;
            result[output_layout.value] = Value::Float64(value);
            Ok(result)
        };
    if let BinaryOpKind::Set(set) = &binary.kind {
        // `and`/`unless` look up the right side; `or` adds unmatched right rows.
        let lookup = if *set == PromQLVectorSetOpKind::Or {
            &left
        } else {
            &right
        };
        let side = usize::from(*set != PromQLVectorSetOpKind::Or);
        let mut signatures = std::collections::BTreeSet::new();
        for row in lookup {
            work.checkpoint().await?;
            let labels = signature(&kind, names, read(side, row)?);
            workspace.grow(label_bytes(&labels))?;
            signatures.insert(labels);
        }
        let and = *set == PromQLVectorSetOpKind::And;
        for row in &left {
            work.checkpoint().await?;
            let found = signatures.contains(&signature(&kind, names, read(0, row)?));
            if *set == PromQLVectorSetOpKind::Or || found == and {
                workspace.grow(row_bytes(row))?;
                result.push(render(0, row, &read(0, row)?, float(&layouts[0], row)?)?);
            }
        }
        if *set == PromQLVectorSetOpKind::Or {
            for row in &right {
                work.checkpoint().await?;
                let labels = read(1, row)?;
                if signatures.contains(&signature(&kind, names, labels.clone())) {
                    continue;
                }
                let out = render(1, row, &labels, float(&layouts[1], row)?)?;
                workspace.grow(row_bytes(&out))?;
                result.push(out);
            }
        }
        return Ok(result);
    }
    // Prometheus returns before matching when either side is empty.
    if left.is_empty() || right.is_empty() {
        return Ok(result);
    }
    // `group_right` makes the left side the "one" side.
    let swapped = matches!(grouping, Some(g) if g.side == GroupSide::Right);
    let (one, many) = if swapped { (0, 1) } else { (1, 0) };
    let sides = [&left, &right];
    let mut ones = BTreeMap::new();
    for (index, row) in sides[one].iter().enumerate() {
        work.checkpoint().await?;
        let labels = read(one, row)?;
        let key = signature(&kind, names, labels.clone());
        workspace.grow(2 * label_bytes(&labels))?;
        if ones
            .insert(key, (labels, float(&layouts[one], row)?, index))
            .is_some()
        {
            return Err(invalid(&format!(
                "found duplicate series for the match group on the {} hand-side of the operation",
                if swapped { "left" } else { "right" }
            )));
        }
    }
    let mut matched = BTreeMap::<Labels, std::collections::BTreeSet<Labels>>::new();
    for row in sides[many].iter() {
        work.checkpoint().await?;
        let labels = read(many, row)?;
        let key = signature(&kind, names, labels.clone());
        let Some((one_labels, one_value, one_index)) = ones.get(&key) else {
            continue;
        };
        let value = float(&layouts[many], row)?;
        let (l, r) = if swapped {
            (*one_value, value)
        } else {
            (value, *one_value)
        };
        let Some(computed) = apply(binary, l, r)? else {
            continue;
        };
        let mut metric = labels;
        if matches!(binary.kind, BinaryOpKind::Arithmetic(_)) {
            metric.remove("__name__");
        }
        match grouping {
            None => match kind {
                VectorMatchKind::On => metric.retain(|k, _| names.contains(k)),
                VectorMatchKind::Ignoring => metric.retain(|k, _| !names.contains(k)),
            },
            // Included labels come from the "one" side.
            Some(g) => {
                for label in &g.labels {
                    match one_labels.get(label) {
                        Some(value) => metric.insert(label.clone(), value.clone()),
                        None => metric.remove(label),
                    };
                }
            }
        }
        if matches!(binary.kind, BinaryOpKind::CompareBool(_)) {
            metric.remove("__name__");
        }
        workspace.grow(label_bytes(&metric))?;
        let results = matched.entry(key).or_default();
        if grouping.is_none() && !results.is_empty() {
            return Err(invalid(
                "many-to-one matching must be explicit (group_left/group_right)",
            ));
        }
        if !results.insert(metric.clone()) {
            return Err(invalid(
                "multiple matches for labels: grouping labels must ensure unique matches",
            ));
        }
        let out = if swapped {
            render(0, &left[*one_index], &metric, computed)?
        } else {
            render(0, row, &metric, computed)?
        };
        workspace.grow(row_bytes(&out))?;
        result.push(out);
    }
    Ok(result)
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let output = operator.output.clone();
    let right = match operator.kind {
        Kind::SeriesBinary { .. } => {
            Some(inputs.pop().ok_or_else(|| invalid("missing right input"))?)
        }
        _ => None,
    };
    let left = inputs.pop().ok_or_else(|| invalid("missing left input"))?;
    Ok(futures::stream::once(async move {
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        let (rows, _memory) = collect_rows(left, &context).await?;
        let mut result = Vec::new();
        match (&operator.kind, right) {
            (
                Kind::SeriesBinary {
                    operator: binary,
                    scalars,
                },
                Some(right),
            ) => {
                let (right, _right_memory) = collect_rows(right, &context).await?;
                result = series_binary(
                    operator,
                    binary,
                    *scalars,
                    [rows, right],
                    &mut work,
                    &mut workspace,
                )
                .await?;
            }
            (
                Kind::SeriesRelabel {
                    destination,
                    replacement,
                    source_regex,
                },
                None,
            ) => {
                let input = &operator.inputs[0];
                let from = layout(input)?;
                let to = layout(&output)?;
                let regex = source_regex
                    .as_ref()
                    .map(|(_, pattern)| regex::Regex::new(&format!("^(?s:{pattern})$")))
                    .transpose()
                    .map_err(|e| invalid(&e.to_string()))?;
                let mut seen = std::collections::BTreeSet::new();
                for row in rows {
                    work.checkpoint().await?;
                    let mut labels = from.read(input, &row)?;
                    let rewritten = match (&source_regex, &regex) {
                        (Some((source, _)), Some(regex)) => regex
                            .captures(labels.get(source).map_or("", String::as_str))
                            .map(|captures| {
                                let mut value = String::new();
                                captures.expand(replacement, &mut value);
                                value
                            }),
                        _ => Some(replacement.clone()),
                    };
                    if let Some(value) = rewritten {
                        if value.is_empty() {
                            labels.remove(destination);
                        } else {
                            labels.insert(destination.clone(), value);
                        }
                    }
                    if !seen.insert(labels.clone()) {
                        return Err(invalid(
                            "vector cannot contain metrics with the same labelset",
                        ));
                    }
                    let mut out = vec![Value::Null; output.fields.len()];
                    if let (Some(from), Some(to)) = (input.time_index, output.time_index) {
                        out[to] = row[from].clone();
                    }
                    out[to.value] = row[from.value].clone();
                    to.write(&output, &mut out, &labels)?;
                    workspace.grow(row_bytes(&out) + label_bytes(&labels))?;
                    result.push(out);
                }
            }
            (Kind::SeriesHistogramQuantile { quantile, le }, None) => {
                let input = &operator.inputs[0];
                let left_layout = layout(input)?;
                let bucket = &input.fields[*le].name;
                let mut histograms = BTreeMap::<Labels, Vec<(f64, f64)>>::new();
                for row in rows {
                    work.checkpoint().await?;
                    let mut set = left_layout.read(input, &row)?;
                    // Prometheus skips a series whose `le` is not a float.
                    let Some(bound) = set.remove(bucket).and_then(|v| parse_bound(&v)) else {
                        continue;
                    };
                    let Value::Float64(count) = row[left_layout.value] else {
                        return Err(invalid("vector value must be Float64"));
                    };
                    workspace.grow(
                        64 + set
                            .iter()
                            .map(|(k, v)| 64 + k.len() + v.len())
                            .sum::<usize>(),
                    )?;
                    // The histogram identity keeps `__name__`; only the result drops it.
                    histograms.entry(set).or_default().push((bound, count));
                }
                let out_layout = layout(&output)?;
                let q = f64::from_bits(*quantile);
                let mut seen = std::collections::BTreeSet::new();
                for (mut set, buckets) in histograms {
                    work.checkpoint().await?;
                    set.remove("__name__");
                    let value = aggregate::temporal::bucket_quantile(q, buckets, &context).await?;
                    let mut row = vec![Value::Null; output.fields.len()];
                    out_layout.write(&output, &mut row, &set)?;
                    row[out_layout.value] = Value::Float64(value);
                    if !seen.insert(set) {
                        return Err(invalid(
                            "vector cannot contain metrics with the same labelset",
                        ));
                    }
                    workspace.grow(row_bytes(&row))?;
                    result.push(row);
                }
            }
            _ => return Err(invalid("series label operator inputs mismatch")),
        }
        Batch::try_new(output.clone(), result)
    })
    .boxed_local())
}
