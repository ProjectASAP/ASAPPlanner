//! PromQL label-set rewriting and binary operators over rows that carry a
//! series identity or plain label columns.
use super::*;
use planner_types::{
    post_asap::BinaryOperator,
    pre_asap::{schema::PROMQL_SERIES_IDENTITY, BinaryOpKind, VectorMatchKind},
};

type Labels = BTreeMap<String, String>;

/// Where a row's label set lives: the encoded series identity when present,
/// otherwise the non-empty Utf8 label columns. The one Float64 column is the
/// sample value, whatever an aggregate named it.
#[derive(Clone, Debug)]
struct Layout {
    identity: Option<usize>,
    labels: Vec<usize>,
    value: usize,
}

fn layout(input: &Schema) -> Result<Layout, Error> {
    let mut identity = None;
    let mut labels = Vec::new();
    let mut value = None;
    for (i, field) in input.fields.iter().enumerate() {
        match plain(input, i)? {
            (DataType::Utf8, false) if field.name == PROMQL_SERIES_IDENTITY => identity = Some(i),
            (DataType::Utf8, _) if field.name != PROMQL_SERIES_IDENTITY => labels.push(i),
            (DataType::Float64, false) if value.is_none() => value = Some(i),
            (DataType::Timestamp, false) if input.time_index == Some(i) => {}
            _ => return Err(invalid("PromQL vector requires labels, time and one value")),
        }
    }
    Ok(Layout {
        identity,
        labels,
        value: value.ok_or_else(|| invalid("PromQL vector requires one value"))?,
    })
}

impl Layout {
    fn read(&self, input: &Schema, row: &[Value]) -> Result<Labels, Error> {
        if let Some(i) = self.identity {
            let Value::Utf8(encoded) = &row[i] else {
                return Err(invalid("series identity must be Utf8"));
            };
            let mut labels: Labels =
                serde_json::from_str(encoded).map_err(|e| invalid(&e.to_string()))?;
            // PromQL treats an empty label value as an absent label.
            labels.retain(|_, v| !v.is_empty());
            return Ok(labels);
        }
        let mut labels = Labels::new();
        for &i in &self.labels {
            match &row[i] {
                Value::Utf8(v) if !v.is_empty() => {
                    labels.insert(input.fields[i].name.clone(), v.to_string());
                }
                Value::Utf8(_) | Value::Null => {}
                _ => return Err(invalid("label must be Utf8")),
            }
        }
        Ok(labels)
    }

    /// Replace the row's labels; a label column absent from `labels` is empty.
    fn write(&self, input: &Schema, row: &mut [Value], labels: &Labels) -> Result<(), Error> {
        if let Some(i) = self.identity {
            let encoded = serde_json::to_string(labels).map_err(|e| invalid(&e.to_string()))?;
            row[i] = Value::Utf8(encoded.into());
        }
        for &i in &self.labels {
            let value = labels.get(&input.fields[i].name).map_or("", String::as_str);
            row[i] = Value::Utf8(value.into());
        }
        Ok(())
    }
}

impl Operator {
    /// Rewrite each row's label set to PromQL's matching labels: `On` keeps
    /// only `labels`; `Ignoring` drops `labels` and the metric name.
    pub fn series_labels(
        input: Schema,
        kind: VectorMatchKind,
        labels: Vec<String>,
    ) -> Result<Self, Error> {
        layout(&input)?;
        Ok(Self {
            kind: Kind::SeriesLabels {
                kind,
                labels,
                unique: false,
            },
            inputs: vec![input.clone()],
            output: input,
        })
    }

    /// Drop the metric name from each row's label set, as PromQL arithmetic
    /// with a literal does. Unlike matching labels, the result is itself a
    /// vector, so two rows that become equal are an error, as in Prometheus.
    pub fn series_without_name(input: Schema) -> Result<Self, Error> {
        layout(&input)?;
        Ok(Self {
            kind: Kind::SeriesLabels {
                kind: VectorMatchKind::Ignoring,
                labels: vec![],
                unique: true,
            },
            inputs: vec![input.clone()],
            output: input,
        })
    }

    /// PromQL `histogram_quantile` over classic buckets: one histogram per
    /// label set other than the `le` column's label. The result drops `le`,
    /// `__name__` and the time column; its value is the quantile.
    pub fn series_histogram_quantile(input: Schema, q: f64, le: usize) -> Result<Self, Error> {
        let layout = layout(&input)?;
        if !layout.labels.contains(&le) {
            return Err(invalid("histogram bucket bound must be a label column"));
        }
        let mut fields = (0..input.fields.len())
            .filter(|&i| i != le && i != layout.value && Some(i) != input.time_index)
            .map(|i| input.fields[i].clone())
            .collect::<Vec<_>>();
        fields.push(input.fields[layout.value].clone());
        Ok(Self {
            kind: Kind::SeriesHistogramQuantile {
                quantile: q.to_bits(),
                le,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }

    /// A PromQL binary operator with Prometheus' matching, metric-name, and
    /// duplicate rules. `scalars` marks the operands that are one-row PromQL
    /// scalars, such as a literal or `scalar(x)`, rather than vectors.
    /// Vectors match by `operator.vector_match`; `None` matches all labels
    /// but the name. The result has the vector operand's schema, the left one
    /// between vectors; label columns without a series identity must hold
    /// every label the result can take from the right side.
    pub fn series_binary(
        left: Schema,
        right: Schema,
        operator: BinaryOperator,
        scalars: [bool; 2],
    ) -> Result<Self, Error> {
        use planner_types::pre_asap::{CompareOpKind::*, GroupSide, PromQLVectorSetOpKind};
        let vectors = scalars == [false, false];
        let valid = match &operator.kind {
            BinaryOpKind::Arithmetic(_) => true,
            BinaryOpKind::CompareBool(op) => matches!(op, Eq | Ne | Lt | Le | Gt | Ge),
            // Prometheus requires `bool` between two scalars.
            BinaryOpKind::Compare(op) => {
                matches!(op, Eq | Ne | Lt | Le | Gt | Ge) && scalars != [true, true]
            }
            BinaryOpKind::Set(_) => vectors,
        };
        let grouping = operator
            .vector_match
            .as_ref()
            .and_then(|m| m.grouping.as_ref());
        // The frontend records a `bool` modifier as default matching.
        let default_match = operator.vector_match.as_ref().is_none_or(|m| {
            m.kind == VectorMatchKind::Ignoring && m.labels.is_empty() && m.grouping.is_none()
        });
        if !valid
            || (!vectors && !default_match)
            || (grouping.is_some() && matches!(operator.kind, BinaryOpKind::Set(_)))
        {
            return Err(invalid("unsupported PromQL binary operation"));
        }
        for (schema, scalar) in [(&left, scalars[0]), (&right, scalars[1])] {
            if scalar {
                if schema.fields.len() != 1 || plain(schema, 0)? != (&DataType::Float64, false) {
                    return Err(invalid("PromQL scalar operand must be one Float64 value"));
                }
            } else {
                layout(schema)?;
            }
        }
        if vectors {
            let (l, r) = (layout(&left)?, layout(&right)?);
            let names = |layout: &Layout, schema: &Schema| {
                layout
                    .labels
                    .iter()
                    .map(|&i| schema.fields[i].name.clone())
                    .collect::<std::collections::BTreeSet<_>>()
            };
            let right_rows = matches!(operator.kind, BinaryOpKind::Set(PromQLVectorSetOpKind::Or))
                || matches!(grouping, Some(g) if g.side == GroupSide::Right);
            let fits = l.identity.is_some()
                || match grouping {
                    _ if right_rows => {
                        r.identity.is_none() && names(&r, &right).is_subset(&names(&l, &left))
                    }
                    Some(g) => g
                        .labels
                        .iter()
                        .all(|label| names(&l, &left).contains(label)),
                    None => true,
                };
            let or = matches!(operator.kind, BinaryOpKind::Set(PromQLVectorSetOpKind::Or));
            // A series identity holds any label set, since `write` re-encodes it.
            // A right row needs a time only if the left layout has one.
            if !fits || (or && left.time_index.is_some() && right.time_index.is_none()) {
                return Err(invalid(
                    "PromQL binary result labels do not fit the left schema",
                ));
            }
        }
        let output = if scalars == [true, false] {
            right.clone()
        } else {
            left.clone()
        };
        Ok(Self {
            kind: Kind::SeriesBinary { operator, scalars },
            inputs: vec![left, right],
            output,
        })
    }
}

/// PromQL's matching signature: `on` keeps only the listed labels; `ignoring`
/// drops them and the metric name.
fn signature(kind: &VectorMatchKind, names: &[String], mut labels: Labels) -> Labels {
    match kind {
        VectorMatchKind::On => labels.retain(|k, _| names.contains(k)),
        VectorMatchKind::Ignoring => labels.retain(|k, _| k != "__name__" && !names.contains(k)),
    }
    labels
}

fn label_bytes(labels: &Labels) -> usize {
    labels.iter().map(|(k, v)| 64 + k.len() + v.len()).sum()
}

fn float(layout: &Layout, row: &[Value]) -> Result<f64, Error> {
    match row[layout.value] {
        Value::Float64(value) => Ok(value),
        _ => Err(invalid("vector value must be Float64")),
    }
}

/// The value of a matched pair, or `None` when a comparison filters it out.
/// A filter keeps the left value.
fn apply(operator: &BinaryOperator, left: f64, right: f64) -> Result<Option<f64>, Error> {
    let unmatched = BinaryOperator {
        vector_match: None,
        ..operator.clone()
    };
    match crate::expressions::arithmetic::evaluate_binary(&unmatched, left, right)? {
        Value::Float64(value) => Ok(Some(value)),
        Value::Bool(keep) => Ok(keep.then_some(left)),
        _ => Err(invalid("PromQL binary result must be a number")),
    }
}

/// Evaluate `Kind::SeriesBinary` over the collected operand rows.
async fn series_binary(
    operator: &Operator,
    binary: &BinaryOperator,
    scalars: [bool; 2],
    rows: [Vec<Vec<Value>>; 2],
    work: &mut Cooperative,
    workspace: &mut Workspace,
) -> Result<Vec<Vec<Value>>, Error> {
    use planner_types::pre_asap::{GroupSide, PromQLVectorSetOpKind};
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
                result.push(row.clone());
            }
        }
        if *set == PromQLVectorSetOpKind::Or {
            for row in &right {
                work.checkpoint().await?;
                let labels = read(1, row)?;
                if signatures.contains(&signature(&kind, names, labels.clone())) {
                    continue;
                }
                let mut out = vec![Value::Null; schemas[0].fields.len()];
                if let (Some(to), Some(from)) = (schemas[0].time_index, schemas[1].time_index) {
                    out[to] = row[from].clone();
                }
                out[layouts[0].value] = Value::Float64(float(&layouts[1], row)?);
                layouts[0].write(schemas[0], &mut out, &labels)?;
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
        // The output row is in the left layout.
        let mut out = if swapped {
            left[*one_index].clone()
        } else {
            row.clone()
        };
        layouts[0].write(schemas[0], &mut out, &metric)?;
        out[layouts[0].value] = Value::Float64(computed);
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
                Kind::SeriesLabels {
                    kind,
                    labels,
                    unique,
                },
                None,
            ) => {
                let left_layout = layout(&operator.inputs[0])?;
                let mut seen = std::collections::BTreeSet::new();
                for mut row in rows {
                    work.checkpoint().await?;
                    let set = signature(kind, labels, left_layout.read(&output, &row)?);
                    left_layout.write(&output, &mut row, &set)?;
                    workspace.grow(row_bytes(&row))?;
                    // Matching and grouping sides may repeat a label set;
                    // their consumers decide whether that is an error.
                    if *unique {
                        workspace.grow(label_bytes(&set))?;
                        if !seen.insert(set) {
                            return Err(invalid(
                                "vector cannot contain metrics with the same labelset",
                            ));
                        }
                    }
                    result.push(row);
                }
            }
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

/// A bucket bound as Go's `strconv.ParseFloat` reads it, except hex floats:
/// an out-of-range literal is an error, not an infinity.
fn parse_bound(text: &str) -> Option<f64> {
    let bound = text.parse::<f64>().ok()?;
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let infinite = ["inf", "infinity"]
        .iter()
        .any(|word| unsigned.eq_ignore_ascii_case(word));
    (bound.is_finite() || bound.is_nan() || infinite).then_some(bound)
}
