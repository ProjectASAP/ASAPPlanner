//! PromQL label-set rewriting and binary operators over rows that carry a
//! series identity or plain label columns.
use super::*;
use crate::expressions::binary::BinaryOpKind;
use crate::expressions::binary::BinaryOperator;
use planner_types::ir::operator::VectorMatchKind;
use planner_types::ir::schema::PROMQL_SERIES_IDENTITY;

pub(super) type Labels = BTreeMap<String, String>;

/// Where a row's label set lives: the encoded series identity when present,
/// otherwise the non-empty Utf8 label columns. The one Float64 column is the
/// sample value, whatever an aggregate named it.
#[derive(Clone, Debug)]
pub(super) struct Layout {
    pub(super) identity: Option<usize>,
    pub(super) labels: Vec<usize>,
    pub(super) value: usize,
}

pub(super) fn layout(input: &SchemaRef) -> Result<Layout, Error> {
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
    #[cfg(test)]
    pub(super) fn read(&self, input: &SchemaRef, row: &[Value]) -> Result<Labels, Error> {
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
    #[cfg(test)]
    pub(super) fn write(
        &self,
        input: &SchemaRef,
        row: &mut [Value],
        labels: &Labels,
    ) -> Result<(), Error> {
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
    pub(crate) fn series_relabel(
        input: SchemaRef,
        output: SchemaRef,
        destination: String,
        replacement: String,
        source_regex: Option<(String, String)>,
    ) -> Result<Self, Error> {
        // Prometheus uses UTF-8 label validation; Rust strings are already UTF-8.
        if destination.is_empty() || destination == PROMQL_SERIES_IDENTITY {
            return Err(invalid("invalid or reserved destination label name"));
        }
        layout(&input)?;
        let out = layout(&output)?;
        if input
            .fields
            .iter()
            .filter(|field| field.name != destination)
            .any(|field| !output.fields.contains(field))
        {
            return Err(invalid(
                "label rewrite must preserve non-destination input columns",
            ));
        }

        if out.identity.is_none()
            && !out
                .labels
                .iter()
                .any(|&i| output.fields[i].name == destination)
        {
            return Err(invalid("label rewrite destination missing from output"));
        }
        if let Some((_, pattern)) = &source_regex {
            crate::expressions::promql_anchored_regex(pattern)
                .map_err(|e| invalid(&e.to_string()))?;
        }
        Ok(Self {
            kind: Kind::SeriesRelabel {
                destination,
                replacement,
                source_regex,
            },
            inputs: vec![input],
            output,
        })
    }

    /// Rewrite each row's label set to PromQL's matching labels: `On` keeps
    /// only `labels`; `Ignoring` drops `labels` and the metric name.
    pub fn series_labels(
        input: SchemaRef,
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
    pub fn series_without_name(input: SchemaRef) -> Result<Self, Error> {
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
    pub fn series_histogram_quantile(input: SchemaRef, q: f64, le: usize) -> Result<Self, Error> {
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
        left: SchemaRef,
        right: SchemaRef,
        operator: BinaryOperator,
        scalars: [bool; 2],
    ) -> Result<Self, Error> {
        use planner_types::ir::operator::{GroupSide, PromQLVectorSetOpKind};
        use planner_types::ir::scalar::CompareOpKind::*;
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
        let mut output = if scalars == [true, false] {
            (*right).clone()
        } else {
            (*left).clone()
        };
        if vectors {
            let r = layout(&right)?;
            let right_rows = matches!(operator.kind, BinaryOpKind::Set(PromQLVectorSetOpKind::Or))
                || matches!(grouping, Some(g) if g.side == GroupSide::Right);
            let mut additions = Vec::new();
            if right_rows {
                additions.extend(
                    r.labels
                        .iter()
                        .chain(r.identity.iter())
                        .map(|&i| right.fields[i].clone()),
                );
            }
            if let Some(grouping) = grouping {
                additions.extend(
                    grouping
                        .labels
                        .iter()
                        .map(|name| result_field(name, DataType::Utf8, true)),
                );
            }
            for field in additions {
                if !output.fields.iter().any(|f| f.name == field.name) {
                    output.fields.push(field);
                }
            }
            let or = matches!(operator.kind, BinaryOpKind::Set(PromQLVectorSetOpKind::Or));
            if or && left.time_index.is_some() && right.time_index.is_none() {
                return Err(invalid("PromQL binary right row requires a timestamp"));
            }
        }
        let output = Arc::new(output);
        Ok(Self {
            kind: Kind::SeriesBinary { operator, scalars },
            inputs: vec![left, right],
            output,
        })
    }
}

/// PromQL's matching signature: `on` keeps only the listed labels; `ignoring`
/// drops them and the metric name.
pub(super) fn signature(kind: &VectorMatchKind, names: &[String], mut labels: Labels) -> Labels {
    match kind {
        VectorMatchKind::On => labels.retain(|k, _| names.contains(k)),
        VectorMatchKind::Ignoring => labels.retain(|k, _| k != "__name__" && !names.contains(k)),
    }
    labels
}

pub(super) fn label_bytes(labels: &Labels) -> usize {
    labels.iter().map(|(k, v)| 64 + k.len() + v.len()).sum()
}

pub(super) fn apply(
    operator: &BinaryOperator,
    left: f64,
    right: f64,
) -> Result<Option<f64>, Error> {
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

/// A bucket bound as Go's `strconv.ParseFloat` reads it, except hex floats:
/// an out-of-range literal is an error, not an infinity.
pub(super) fn parse_bound(text: &str) -> Option<f64> {
    let bound = text.parse::<f64>().ok()?;
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let infinite = ["inf", "infinity"]
        .iter()
        .any(|word| unsigned.eq_ignore_ascii_case(word));
    (bound.is_finite() || bound.is_nan() || infinite).then_some(bound)
}

/// One operand's series: each row's label set (by its label source, a
/// series identity or a tuple of label columns, read once per batch), its
/// value and its time.
struct SeriesRows {
    /// Each row's label source.
    source_of_row: Vec<u32>,
    /// Each source's label set, as `Layout::read` reads it.
    sets: Vec<Labels>,
    /// Each row's value.
    values: arrow::array::Float64Array,
    /// Each row's time, when the schema has a time column.
    times: Option<arrow::array::TimestampMillisecondArray>,
}

impl SeriesRows {
    fn labels(&self, row: usize) -> &Labels {
        &self.sets[self.source_of_row[row] as usize]
    }
    fn value(&self, row: usize) -> f64 {
        self.values.value(row)
    }
    fn time(&self, row: usize) -> Option<i64> {
        self.times.as_ref().map(|times| times.value(row))
    }
    fn len(&self) -> usize {
        self.source_of_row.len()
    }
}

/// Each row's label source and each source's label set (`Layout::read`):
/// the parsed series identity (empty values dropped), or the non-empty label
/// columns.
fn label_sources(
    batch: &Batch,
    layout: &Layout,
    schema: &SchemaRef,
) -> Result<(Vec<u32>, Vec<Labels>), Error> {
    use arrow::array::{DictionaryArray, StringArray};
    use arrow::datatypes::UInt32Type;
    let rows = batch.num_rows();
    match layout.identity {
        Some(identity) => {
            let dictionary = batch
                .column(identity)
                .as_any()
                .downcast_ref::<DictionaryArray<UInt32Type>>()
                .expect("batch text columns are dictionaries");
            let texts = dictionary
                .values()
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("dictionaries over strings");
            let keys = dictionary.keys();
            let mut source_of_entry = ahash::AHashMap::<u32, u32>::new();
            let mut sets = Vec::new();
            let mut source_of_row = Vec::with_capacity(rows);
            for row in 0..rows {
                let entry = keys.value(row);
                let source = match source_of_entry.get(&entry) {
                    Some(&source) => source,
                    None => {
                        let mut labels: Labels = serde_json::from_str(texts.value(entry as usize))
                            .map_err(|e| invalid(&e.to_string()))?;
                        // PromQL treats an empty label value as an absent label.
                        labels.retain(|_, v| !v.is_empty());
                        sets.push(labels);
                        let source = (sets.len() - 1) as u32;
                        source_of_entry.insert(entry, source);
                        source
                    }
                };
                source_of_row.push(source);
            }
            Ok((source_of_row, sets))
        }
        None => {
            let mut table = super::group_table::GroupTable::new(layout.labels.clone());
            let source_of_row = table.group_ids(batch)?;
            let sets = (0..table.len() as u32)
                .map(|group| {
                    let mut labels = Labels::new();
                    for (index, &column) in layout.labels.iter().enumerate() {
                        match table.label(group, index) {
                            Value::Utf8(v) if !v.is_empty() => {
                                labels.insert(schema.fields[column].name.clone(), v.to_string());
                            }
                            Value::Utf8(_) | Value::Null => {}
                            _ => return Err(invalid("label must be Utf8")),
                        }
                    }
                    Ok(labels)
                })
                .collect::<Result<Vec<_>, Error>>()?;
            Ok((source_of_row, sets))
        }
    }
}

fn series_rows(batch: &Batch, schema: &SchemaRef) -> Result<SeriesRows, Error> {
    let layout = layout(schema)?;
    let (source_of_row, sets) = label_sources(batch, &layout, schema)?;
    let values = batch
        .column(layout.value)
        .as_any()
        .downcast_ref::<arrow::array::Float64Array>()
        .cloned()
        .ok_or_else(|| invalid("vector value must be Float64"))?;
    let times = schema.time_index.map(|index| {
        batch
            .column(index)
            .as_any()
            .downcast_ref::<arrow::array::TimestampMillisecondArray>()
            .expect("series timestamps are timestamps")
            .clone()
    });
    Ok(SeriesRows {
        source_of_row,
        sets,
        values,
        times,
    })
}

/// A dictionary text column with row `i` holding `texts[i]`.
fn interned_texts(texts: impl Iterator<Item = String>) -> Result<arrow::array::ArrayRef, Error> {
    let mut codes = ahash::AHashMap::<String, u32>::new();
    let mut values: Vec<String> = Vec::new();
    let keys = texts
        .map(|text| {
            let next = values.len() as u32;
            *codes.entry(text.clone()).or_insert_with(|| {
                values.push(text);
                next
            })
        })
        .collect::<arrow::array::UInt32Array>();
    Ok(Arc::new(
        arrow::array::DictionaryArray::<arrow::datatypes::UInt32Type>::try_new(
            keys,
            Arc::new(arrow::array::StringArray::from(values)),
        )
        .map_err(|e| invalid(&e.to_string()))?,
    ))
}

/// The batch of `output` holding `rows`, each (time, label set, value),
/// written as `Layout::write` writes a label set: the identity as the set's
/// canonical text, each label column its label or empty.
fn render(output: &SchemaRef, rows: &[(Option<i64>, Labels, f64)]) -> Result<Batch, Error> {
    let to = layout(output)?;
    let mut arrays: Vec<Option<arrow::array::ArrayRef>> = vec![None; output.fields.len()];
    if let Some(identity) = to.identity {
        arrays[identity] = Some(interned_texts(
            rows.iter()
                .map(|(_, labels, _)| {
                    serde_json::to_string(labels).map_err(|e| invalid(&e.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter(),
        )?);
    }
    for &column in &to.labels {
        let name = &output.fields[column].name;
        arrays[column] =
            Some(interned_texts(rows.iter().map(|(_, labels, _)| {
                labels.get(name).cloned().unwrap_or_default()
            }))?);
    }
    if let Some(index) = output.time_index {
        arrays[index] = Some(Arc::new(
            rows.iter()
                .map(|(time, _, _)| *time)
                .collect::<arrow::array::TimestampMillisecondArray>(),
        ));
    }
    arrays[to.value] = Some(Arc::new(arrow::array::Float64Array::from_iter_values(
        rows.iter().map(|(_, _, value)| *value),
    )));
    let arrays = arrays
        .into_iter()
        .map(|array| {
            array.ok_or_else(|| invalid("PromQL vector requires labels, time and one value"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Batch::from_parts(
        output.clone(),
        arrays,
        vec![None; output.fields.len()],
        rows.len(),
    )
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let right = match operator.kind {
        Kind::SeriesBinary { .. } => {
            Some(inputs.pop().ok_or_else(|| invalid("missing right input"))?)
        }
        _ => None,
    };
    let left = inputs.pop().ok_or_else(|| invalid("missing left input"))?;
    if let Kind::SeriesLabels {
        kind,
        labels,
        unique,
    } = &operator.kind
    {
        return Ok(futures::stream::once(rewrite_label_sets(
            left, operator, kind, labels, *unique, context,
        ))
        .boxed_local());
    }
    Ok(futures::stream::once(async move {
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        let output = &operator.output;
        let left = collect_batch(left, &operator.inputs[0], &context).await?;
        let result = match (&operator.kind, right) {
            (
                Kind::SeriesBinary {
                    operator: binary,
                    scalars,
                },
                Some(right),
            ) => {
                let right = collect_batch(right, &operator.inputs[1], &context).await?;
                series_binary(
                    operator,
                    binary,
                    *scalars,
                    [&left.batch, &right.batch],
                    &mut work,
                    &mut workspace,
                )
                .await?
            }
            (
                Kind::SeriesRelabel {
                    destination,
                    replacement,
                    source_regex,
                },
                None,
            ) => {
                let rows = series_rows(&left.batch, &operator.inputs[0])?;
                let regex = source_regex
                    .as_ref()
                    .map(|(_, pattern)| crate::expressions::promql_anchored_regex(pattern))
                    .transpose()
                    .map_err(|e| invalid(&e.to_string()))?;
                // Each source's relabelled set, once.
                let relabelled = rows
                    .sets
                    .iter()
                    .map(|labels| {
                        let mut labels = labels.clone();
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
                        labels
                    })
                    .collect::<Vec<_>>();
                let mut seen = std::collections::BTreeSet::new();
                let mut out = Vec::with_capacity(rows.len());
                for row in 0..rows.len() {
                    let labels = &relabelled[rows.source_of_row[row] as usize];
                    if !seen.insert(labels) {
                        return Err(invalid(
                            "vector cannot contain metrics with the same labelset",
                        ));
                    }
                    workspace.grow(label_bytes(labels) + 32)?;
                    out.push((rows.time(row), labels.clone(), rows.value(row)));
                }
                work.checkpoint_rows(rows.len()).await?;
                render(output, &out)?
            }
            (Kind::SeriesHistogramQuantile { quantile, le }, None) => {
                let input = &operator.inputs[0];
                let rows = series_rows(&left.batch, input)?;
                let bucket = &input.fields[*le].name;
                let mut histograms = BTreeMap::<Labels, Vec<(f64, f64)>>::new();
                for row in 0..rows.len() {
                    let mut set = rows.labels(row).clone();
                    // Prometheus skips a series whose `le` is not a float.
                    let Some(bound) = set.remove(bucket).and_then(|v| parse_bound(&v)) else {
                        continue;
                    };
                    workspace.grow(
                        64 + set
                            .iter()
                            .map(|(k, v)| 64 + k.len() + v.len())
                            .sum::<usize>(),
                    )?;
                    // The histogram identity keeps `__name__`; only the result drops it.
                    histograms
                        .entry(set)
                        .or_default()
                        .push((bound, rows.value(row)));
                }
                work.checkpoint_rows(rows.len()).await?;
                let q = f64::from_bits(*quantile);
                let mut seen = std::collections::BTreeSet::new();
                let mut out = Vec::new();
                for (mut set, buckets) in histograms {
                    work.checkpoint().await?;
                    set.remove("__name__");
                    let value = aggregate::temporal::bucket_quantile(q, buckets, &context).await?;
                    if !seen.insert(set.clone()) {
                        return Err(invalid(
                            "vector cannot contain metrics with the same labelset",
                        ));
                    }
                    out.push((None, set, value));
                }
                render(output, &out)?
            }
            _ => return Err(invalid("series label operator inputs mismatch")),
        };
        workspace.grow(result.memory_bytes())?;
        Ok(result)
    })
    .boxed_local())
}

/// The value of a PromQL scalar operand: its one row's value.
fn scalar_value(batch: &Batch) -> Result<f64, Error> {
    if batch.num_rows() != 1 {
        return Err(invalid("PromQL scalar operand must have one row"));
    }
    match decoded(batch, 0)?.remove(0) {
        Value::Float64(value) => Ok(value),
        _ => Err(invalid("PromQL scalar must be Float64")),
    }
}

/// `Kind::SeriesBinary` over the operands' batches: the row operator's
/// matching, computed from each row's label set, value and time.
async fn series_binary(
    operator: &Operator,
    binary: &BinaryOperator,
    scalars: [bool; 2],
    batches: [&Batch; 2],
    work: &mut Cooperative,
    workspace: &mut Workspace,
) -> Result<Batch, Error> {
    use planner_types::ir::operator::{GroupSide, PromQLVectorSetOpKind};
    let output = &operator.output;
    let drops_name = matches!(
        binary.kind,
        BinaryOpKind::Arithmetic(_) | BinaryOpKind::CompareBool(_)
    );
    if scalars == [true, true] {
        let value = apply(binary, scalar_value(batches[0])?, scalar_value(batches[1])?)?
            .ok_or_else(|| invalid("scalar comparison requires bool"))?;
        return Batch::from_parts(
            output.clone(),
            vec![Arc::new(arrow::array::Float64Array::from(vec![value]))],
            vec![None],
            1,
        );
    }
    if scalars[0] || scalars[1] {
        let (constant, vector, schema) = if scalars[0] {
            (scalar_value(batches[0])?, batches[1], &operator.inputs[1])
        } else {
            (scalar_value(batches[1])?, batches[0], &operator.inputs[0])
        };
        let rows = series_rows(vector, schema)?;
        let mut seen = std::collections::BTreeSet::new();
        // Kept rows and their values; with the name dropped, their sets.
        let (mut kept, mut computed_values, mut rendered) = (Vec::new(), Vec::new(), Vec::new());
        for row in 0..rows.len() {
            let value = rows.value(row);
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
            let mut set = rows.labels(row).clone();
            if drops_name {
                set.remove("__name__");
                rendered.push((rows.time(row), set.clone(), computed));
            }
            workspace.grow(label_bytes(&set) + 32)?;
            if !seen.insert(set) {
                return Err(invalid(
                    "vector cannot contain metrics with the same labelset",
                ));
            }
            kept.push(row as u32);
            computed_values.push(computed);
        }
        work.checkpoint_rows(rows.len()).await?;
        if drops_name {
            return render(output, &rendered);
        }
        // The kept rows as they were, with their computed values.
        let layout = layout(schema)?;
        let taken = vector.take(&arrow::array::UInt32Array::from(kept))?;
        let mut arrays = taken.record_batch().columns().to_vec();
        arrays[layout.value] = Arc::new(arrow::array::Float64Array::from(computed_values));
        return Batch::from_parts(
            output.clone(),
            arrays,
            vec![None; output.fields.len()],
            taken.num_rows(),
        );
    }
    let sides = [
        series_rows(batches[0], &operator.inputs[0])?,
        series_rows(batches[1], &operator.inputs[1])?,
    ];
    let (kind, names, grouping) = match &binary.vector_match {
        None => (VectorMatchKind::Ignoring, &[][..], None),
        Some(m) => (m.kind.clone(), m.labels.as_slice(), m.grouping.as_ref()),
    };
    // An output row from row `row` of side `side`: its time, a set, a value.
    let at = |side: usize, row: usize| {
        if output.time_index.is_some() && operator.inputs[side].time_index.is_some() {
            sides[side].time(row)
        } else {
            None
        }
    };
    let mut result = Vec::new();
    if let BinaryOpKind::Set(set) = &binary.kind {
        // `and`/`unless` look up the right side; `or` adds unmatched right rows.
        let side = usize::from(*set != PromQLVectorSetOpKind::Or);
        let lookup = &sides[side];
        let mut signatures = std::collections::BTreeSet::new();
        for row in 0..lookup.len() {
            let labels = signature(&kind, names, lookup.labels(row).clone());
            workspace.grow(label_bytes(&labels))?;
            signatures.insert(labels);
        }
        work.checkpoint_rows(lookup.len()).await?;
        let and = *set == PromQLVectorSetOpKind::And;
        for row in 0..sides[0].len() {
            let found = signatures.contains(&signature(&kind, names, sides[0].labels(row).clone()));
            if *set == PromQLVectorSetOpKind::Or || found == and {
                workspace.grow(label_bytes(sides[0].labels(row)) + 32)?;
                result.push((
                    at(0, row),
                    sides[0].labels(row).clone(),
                    sides[0].value(row),
                ));
            }
        }
        work.checkpoint_rows(sides[0].len()).await?;
        if *set == PromQLVectorSetOpKind::Or {
            for row in 0..sides[1].len() {
                let labels = sides[1].labels(row);
                if signatures.contains(&signature(&kind, names, labels.clone())) {
                    continue;
                }
                workspace.grow(label_bytes(labels) + 32)?;
                result.push((at(1, row), labels.clone(), sides[1].value(row)));
            }
            work.checkpoint_rows(sides[1].len()).await?;
        }
        return render(output, &result);
    }
    // Prometheus returns before matching when either side is empty.
    if sides[0].len() == 0 || sides[1].len() == 0 {
        return render(output, &result);
    }
    // `group_right` makes the left side the "one" side.
    let swapped = matches!(grouping, Some(g) if g.side == GroupSide::Right);
    let (one, many) = if swapped { (0, 1) } else { (1, 0) };
    let mut ones = BTreeMap::new();
    for row in 0..sides[one].len() {
        let labels = sides[one].labels(row).clone();
        let key = signature(&kind, names, labels.clone());
        workspace.grow(2 * label_bytes(&labels))?;
        if ones
            .insert(key, (labels, sides[one].value(row), row))
            .is_some()
        {
            return Err(invalid(&format!(
                "found duplicate series for the match group on the {} hand-side of the operation",
                if swapped { "left" } else { "right" }
            )));
        }
    }
    work.checkpoint_rows(sides[one].len()).await?;
    let mut matched = BTreeMap::<Labels, std::collections::BTreeSet<Labels>>::new();
    for row in 0..sides[many].len() {
        let labels = sides[many].labels(row).clone();
        let key = signature(&kind, names, labels.clone());
        let Some((one_labels, one_value, one_index)) = ones.get(&key) else {
            continue;
        };
        let value = sides[many].value(row);
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
        // The output row takes its time from the left side's row.
        let time = if swapped {
            at(0, *one_index)
        } else {
            at(0, row)
        };
        workspace.grow(label_bytes(&metric) + 32)?;
        result.push((time, metric, computed));
    }
    work.checkpoint_rows(sides[many].len()).await?;
    render(output, &result)
}

/// `SeriesLabels` over column batches: each distinct label set (a series
/// identity, or a tuple of label columns) is rewritten once per batch, and
/// its new identity and label values are dictionary entries shared by its
/// rows. Other columns are shared, not copied.
async fn rewrite_label_sets(
    mut input: Input<'_, Batch>,
    operator: &Operator,
    kind: &VectorMatchKind,
    names: &[String],
    unique: bool,
    context: RunContext,
) -> Result<Batch, Error> {
    use super::group_table::GroupTable;
    use arrow::array::{
        Array, DictionaryArray, StringArray, TimestampMillisecondArray, UInt32Array,
    };
    use arrow::datatypes::UInt32Type;
    let output = operator.output.clone();
    let layout = layout(&operator.inputs[0])?;
    let mut work = Cooperative::new(&context);
    let mut workspace = Workspace::new(&context)?;
    // Each distinct label set seen, numbered, and each (time, label set)
    // a row of the vector has had.
    let mut label_set_ids = ahash::AHashMap::<String, u32>::new();
    let mut seen = ahash::AHashSet::<(Option<i64>, u32)>::new();
    let mut batches = Vec::new();
    while let Some(batch) = input.next().await {
        let batch = batch?;
        let rows = batch.num_rows();
        // Each row's distinct label source, and each distinct source's
        // rewritten label set.
        let (source_of_row, sets): (Vec<u32>, Vec<Labels>) = match layout.identity {
            Some(identity) => {
                let dictionary = batch
                    .column(identity)
                    .as_any()
                    .downcast_ref::<DictionaryArray<UInt32Type>>()
                    .expect("batch text columns are dictionaries");
                let texts = dictionary
                    .values()
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("dictionaries over strings");
                let keys = dictionary.keys();
                let mut source_of_entry = ahash::AHashMap::<u32, u32>::new();
                let mut sets = Vec::new();
                let mut source_of_row = Vec::with_capacity(rows);
                for row in 0..rows {
                    let entry = keys.value(row);
                    let source = match source_of_entry.get(&entry) {
                        Some(&source) => source,
                        None => {
                            let mut labels: Labels =
                                serde_json::from_str(texts.value(entry as usize))
                                    .map_err(|e| invalid(&e.to_string()))?;
                            // PromQL treats an empty label value as an absent label.
                            labels.retain(|_, v| !v.is_empty());
                            sets.push(signature(kind, names, labels));
                            let source = (sets.len() - 1) as u32;
                            source_of_entry.insert(entry, source);
                            source
                        }
                    };
                    source_of_row.push(source);
                }
                (source_of_row, sets)
            }
            None => {
                let mut table = GroupTable::new(layout.labels.clone());
                let source_of_row = table.group_ids(&batch)?;
                let sets = (0..table.len() as u32)
                    .map(|group| {
                        let mut labels = Labels::new();
                        for (index, &column) in layout.labels.iter().enumerate() {
                            match table.label(group, index) {
                                Value::Utf8(v) if !v.is_empty() => {
                                    labels
                                        .insert(output.fields[column].name.clone(), v.to_string());
                                }
                                Value::Utf8(_) | Value::Null => {}
                                _ => return Err(invalid("label must be Utf8")),
                            }
                        }
                        Ok(signature(kind, names, labels))
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                (source_of_row, sets)
            }
        };
        work.checkpoint_rows(rows).await?;
        // A text column whose row `i` reads `text(source_of_row[i])`.
        let text_column = |text: &dyn Fn(&Labels) -> Result<String, Error>| -> Result<arrow::array::ArrayRef, Error> {
            let mut codes = ahash::AHashMap::<String, u32>::new();
            let mut values: Vec<String> = Vec::new();
            let code_of_source = sets
                .iter()
                .map(|set| {
                    let text = text(set)?;
                    let next = values.len() as u32;
                    Ok(*codes.entry(text.clone()).or_insert_with(|| {
                        values.push(text);
                        next
                    }))
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let keys = UInt32Array::from_iter_values(source_of_row.iter().map(|&s| code_of_source[s as usize]));
            Ok(Arc::new(
                DictionaryArray::<UInt32Type>::try_new(keys, Arc::new(StringArray::from(values)))
                    .map_err(|e| invalid(&e.to_string()))?,
            ))
        };
        let mut arrays = batch.record_batch().columns().to_vec();
        if let Some(identity) = layout.identity {
            arrays[identity] = text_column(&|set| {
                serde_json::to_string(set).map_err(|e| invalid(&e.to_string()))
            })?;
        }
        for &column in &layout.labels {
            let name = &output.fields[column].name;
            arrays[column] = text_column(&|set| Ok(set.get(name).cloned().unwrap_or_default()))?;
        }
        let states = (0..arrays.len())
            .map(|column| batch.state_column(column).cloned())
            .collect();
        let rewritten = Batch::from_parts(output.clone(), arrays, states, rows)?;
        workspace.grow(rewritten.memory_bytes())?;
        // Matching and grouping sides may repeat a label set; their
        // consumers decide whether that is an error.
        if unique {
            let ids = sets
                .iter()
                .map(|set| {
                    let key = serde_json::to_string(set).map_err(|e| invalid(&e.to_string()))?;
                    let next = label_set_ids.len() as u32;
                    Ok(*label_set_ids.entry(key).or_insert(next))
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let times = output.time_index.map(|index| {
                rewritten
                    .column(index)
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .expect("series timestamps are timestamps")
                    .clone()
            });
            for (row, &source) in source_of_row.iter().enumerate() {
                let time = times.as_ref().map(|times| times.value(row));
                workspace.grow(label_bytes(&sets[source as usize]))?;
                if !seen.insert((time, ids[source as usize])) {
                    return Err(invalid(
                        "vector cannot contain metrics with the same labelset",
                    ));
                }
            }
            work.checkpoint_rows(rows).await?;
        }
        batches.push(rewritten);
    }
    if batches.len() == 1 {
        return Ok(batches.remove(0));
    }
    // The one output batch is a copy of the rewritten batches.
    let joined = concat(&output, batches)?;
    workspace.grow(joined.memory_bytes())?;
    Ok(joined)
}

/// One batch of `batches` (all of `schema`, plain columns only), for an
/// operator that emits one batch.
fn concat(schema: &SchemaRef, batches: Vec<Batch>) -> Result<Batch, Error> {
    if batches.is_empty() {
        return Batch::try_new(schema.clone(), vec![]);
    }
    let rows = batches.iter().map(Batch::num_rows).sum();
    let columns = (0..schema.fields.len())
        .map(|column| {
            let arrays = batches
                .iter()
                .map(|batch| batch.column(column).as_ref())
                .collect::<Vec<_>>();
            arrow::compute::concat(&arrays).map_err(|e| invalid(&e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Batch::from_parts(
        schema.clone(),
        columns,
        vec![None; schema.fields.len()],
        rows,
    )
}

#[cfg(test)]
mod columnar_tests {
    use super::*;
    use crate::plan::PhysicalDAG;
    use crate::runtime::{Limits, Scope};
    use planner_types::ir::schema::Field;

    /// The row-at-a-time rewrite this operator ran before (the reference).
    fn reference(
        input: &SchemaRef,
        kind: &VectorMatchKind,
        names: &[String],
        unique: bool,
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Value>>, Error> {
        let layout = layout(input)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut result = Vec::new();
        for mut row in rows {
            let set = signature(kind, names, layout.read(input, &row)?);
            layout.write(input, &mut row, &set)?;
            if unique {
                let time = input.time_index.map(|index| match row[index] {
                    Value::Timestamp(time) => time,
                    _ => unreachable!(),
                });
                if !seen.insert((time, set)) {
                    return Err(invalid(
                        "vector cannot contain metrics with the same labelset",
                    ));
                }
            }
            result.push(row);
        }
        Ok(result)
    }

    fn field(name: &str, dtype: DataType, nullable: bool) -> Field {
        Field {
            name: name.into(),
            dtype: SummaryFamilyType::Plain(dtype),
            nullable,
            table: None,
        }
    }

    // Rewriting label sets column by column equals the row rewrite, for
    // series identities and for label columns, `on` and `ignoring`, over
    // several batches; a repeated label set in a vector is the same error.
    #[test]
    fn label_sets_rewrite_as_rows_did() {
        let text = |s: &str| Value::Utf8(s.into());
        let identity = |name: &str, job: &str, instance: &str| {
            let labels = Labels::from([
                ("__name__".to_string(), name.to_string()),
                ("job".to_string(), job.to_string()),
                ("instance".to_string(), instance.to_string()),
            ]);
            text(&serde_json::to_string(&labels).unwrap())
        };
        let with_identity = Arc::new(Schema {
            fields: vec![
                field("ts", DataType::Timestamp, false),
                field("value", DataType::Float64, false),
                field(PROMQL_SERIES_IDENTITY, DataType::Utf8, false),
                field("job", DataType::Utf8, true),
            ],
            time_index: Some(0),
            unique_keys: vec![],
            closed: false,
        });
        let identity_rows = |metric: &str| {
            (0..12)
                .map(|i| {
                    vec![
                        Value::Timestamp(i / 4),
                        Value::Float64(i as f64),
                        identity(
                            metric,
                            ["api", "web", ""][i as usize % 3],
                            &format!("h{}", i % 2),
                        ),
                        if i % 5 == 0 { Value::Null } else { text("x") },
                    ]
                })
                .collect::<Vec<_>>()
        };
        let labels_only = Arc::new(Schema {
            fields: vec![
                field("value", DataType::Float64, false),
                field("job", DataType::Utf8, true),
                field("__name__", DataType::Utf8, false),
            ],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        let label_rows = (0..9)
            .map(|i| {
                vec![
                    Value::Float64(i as f64),
                    if i % 4 == 0 {
                        Value::Null
                    } else {
                        text(["api", "", "web"][i % 3])
                    },
                    text(["m", "n"][i % 2]),
                ]
            })
            .collect::<Vec<_>>();
        // Two metrics of the same series: equal once the name is dropped.
        let two_metrics = [identity_rows("up"), identity_rows("down")].concat();
        let mut errors = 0;
        for (schema, rows) in [
            (with_identity.clone(), identity_rows("up")),
            (with_identity.clone(), two_metrics),
            (labels_only.clone(), label_rows),
        ] {
            for (kind, names, unique) in [
                (VectorMatchKind::Ignoring, vec![], true),
                (
                    VectorMatchKind::Ignoring,
                    vec!["instance".to_string()],
                    false,
                ),
                (VectorMatchKind::On, vec!["job".to_string()], false),
                (VectorMatchKind::On, vec!["job".to_string()], true),
            ] {
                let mut operator =
                    Operator::series_labels(schema.clone(), kind.clone(), names.clone()).unwrap();
                if let Kind::SeriesLabels { unique: slot, .. } = &mut operator.kind {
                    *slot = unique;
                }
                let expected = reference(&schema, &kind, &names, unique, rows.clone());
                let (first, second) = rows.split_at(rows.len() / 2);
                let mut dag = PhysicalDAG::default();
                dag.add(
                    0,
                    vec![],
                    Operator::source(
                        schema.clone(),
                        vec![
                            Batch::try_new(schema.clone(), first.to_vec()).unwrap(),
                            Batch::try_new(schema.clone(), second.to_vec()).unwrap(),
                        ],
                    )
                    .unwrap(),
                )
                .unwrap();
                dag.add(1, vec![0], operator).unwrap();
                let context = RunContext::new(
                    Scope::Query {
                        evaluation_time_ms: 0,
                    },
                    Limits::default(),
                )
                .unwrap();
                let actual = futures::executor::block_on(async {
                    let mut output = dag.execute(&[1], context).unwrap().remove(0);
                    let mut rows = Vec::new();
                    while let Some(batch) = output.next().await {
                        rows.extend(batch?.to_rows().iter().cloned());
                    }
                    Ok::<_, Error>(rows)
                });
                let case = format!(
                    "{:?} {kind:?} {names:?} unique {unique}",
                    schema.fields.len()
                );
                match (expected, actual) {
                    (Ok(e), Ok(a)) => assert_eq!(format!("{e:?}"), format!("{a:?}"), "{case}"),
                    (Err(_), Err(a)) => {
                        errors += 1;
                        assert!(a.to_string().contains("same labelset"), "{case}: {a}")
                    }
                    (e, a) => panic!("{case}: reference {e:?}, columnar {a:?}"),
                }
            }
        }
        assert!(errors > 0, "the repeated-label-set error is exercised");
    }
}
