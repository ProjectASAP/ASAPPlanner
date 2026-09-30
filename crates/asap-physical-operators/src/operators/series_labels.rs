//! PromQL label-set rewriting and one-to-one vector matching over rows that
//! carry a series identity or plain label columns.
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

    /// PromQL one-to-one arithmetic between rows with equal label sets. The
    /// result keeps the left row, without the metric name.
    pub fn series_binary(
        left: Schema,
        right: Schema,
        operator: BinaryOperator,
    ) -> Result<Self, Error> {
        layout(&left)?;
        layout(&right)?;
        if !matches!(operator.kind, BinaryOpKind::Arithmetic(_)) || operator.vector_match.is_some()
        {
            return Err(invalid("series binary requires unmatched arithmetic"));
        }
        Ok(Self {
            kind: Kind::SeriesBinary { operator },
            inputs: vec![left.clone(), right],
            output: left,
        })
    }
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let output = operator.output.clone();
    let left_layout = layout(&operator.inputs[0])?;
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
                let mut seen = std::collections::BTreeSet::new();
                for mut row in rows {
                    work.checkpoint().await?;
                    let mut set = left_layout.read(&output, &row)?;
                    match kind {
                        VectorMatchKind::On => set.retain(|k, _| labels.contains(k)),
                        VectorMatchKind::Ignoring => {
                            set.retain(|k, _| k != "__name__" && !labels.contains(k))
                        }
                    }
                    left_layout.write(&output, &mut row, &set)?;
                    workspace.grow(row_bytes(&row))?;
                    // Matching and grouping sides may repeat a label set;
                    // their consumers decide whether that is an error.
                    if *unique {
                        workspace.grow(set.iter().map(|(k, v)| 64 + k.len() + v.len()).sum())?;
                        if !seen.insert(set) {
                            return Err(invalid(
                                "vector cannot contain metrics with the same labelset",
                            ));
                        }
                    }
                    result.push(row);
                }
            }
            (Kind::SeriesBinary { operator: binary }, Some(right)) => {
                let right_schema = &operator.inputs[1];
                let right_layout = layout(right_schema)?;
                let (right, _right_memory) = collect_rows(right, &context).await?;
                // Prometheus returns before matching when either side is empty.
                if rows.is_empty() || right.is_empty() {
                    return Batch::try_new(output.clone(), vec![]);
                }
                let mut matches = BTreeMap::new();
                for row in &right {
                    work.checkpoint().await?;
                    let set = right_layout.read(right_schema, row)?;
                    workspace.grow(set.iter().map(|(k, v)| 64 + k.len() + v.len()).sum())?;
                    let Value::Float64(value) = row[right_layout.value] else {
                        return Err(invalid("vector value must be Float64"));
                    };
                    // Prometheus rejects a duplicate on the one side.
                    if matches.insert(set, (value, false)).is_some() {
                        return Err(invalid(
                            "duplicate series for a match group on the right-hand side",
                        ));
                    }
                }
                for mut row in rows {
                    work.checkpoint().await?;
                    let mut set = left_layout.read(&output, &row)?;
                    let Some((value, matched)) = matches.get_mut(&set) else {
                        continue;
                    };
                    // Only a left duplicate that finds a match is ambiguous.
                    if std::mem::replace(matched, true) {
                        return Err(invalid(
                            "many-to-one matching must be explicit (group_left/group_right)",
                        ));
                    }
                    let Value::Float64(left_value) = row[left_layout.value] else {
                        return Err(invalid("vector value must be Float64"));
                    };
                    row[left_layout.value] = crate::expressions::arithmetic::evaluate_binary(
                        binary, left_value, *value,
                    )?;
                    set.remove("__name__");
                    left_layout.write(&output, &mut row, &set)?;
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
