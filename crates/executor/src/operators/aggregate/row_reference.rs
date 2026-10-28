//! The row-at-a-time exact `Aggregate` this crate ran before the columnar
//! one, kept as the reference its tests compare against.
#![cfg(test)]
use super::*;
use crate::operators::common::{key_bytes, row_bytes, Workspace};
use crate::runtime::Cooperative;
use crate::values::group_key;

pub(super) async fn reduce(
    rows: Vec<Vec<Value>>,
    groups: &[usize],
    measures: &[Reduction],
    filters: &[Option<Expression>],
    input: &SchemaRef,
    context: &RunContext,
) -> Result<Vec<Vec<Value>>, Error> {
    let mut work = Cooperative::new(context);
    let mut workspace = Workspace::new(context)?;
    let mut grouped = BTreeMap::<Vec<Vec<u8>>, Vec<Vec<Value>>>::new();
    // PromQL sums over an empty vector emit no sample.
    if rows.is_empty() && groups.is_empty() && !input.has_promql_series_identity() {
        grouped.insert(vec![], vec![]);
    }
    for row in rows {
        work.checkpoint().await?;
        let key = group_key(&row, groups)?;
        workspace.grow(std::mem::size_of::<Vec<Value>>())?;
        if !grouped.contains_key(&key) {
            workspace.grow(key_bytes(&key))?;
        }
        grouped.entry(key).or_default().push(row);
    }
    let mut output = Vec::new();
    for rows in grouped.into_values() {
        work.checkpoint().await?;
        let mut result = groups
            .iter()
            .map(|&i| rows[0][i].clone())
            .collect::<Vec<_>>();
        for (index, measure) in measures.iter().enumerate() {
            let mut selected = Vec::with_capacity(rows.len());
            for row in &rows {
                // SQL FILTER keeps a row only when the predicate is true, not NULL.
                if filters
                    .get(index)
                    .and_then(Option::as_ref)
                    .map_or(Ok(true), |filter| {
                        filter.evaluate(row).map(|v| matches!(v, Value::Bool(true)))
                    })?
                {
                    selected.push(row);
                }
            }
            result.push(reduce_one(&selected, measure, input, &mut work, context).await?);
        }
        workspace.grow(row_bytes(&result))?;
        output.push(result);
    }
    Ok(output)
}

async fn reduce_one(
    rows: &[&Vec<Value>],
    measure: &Reduction,
    input: &SchemaRef,
    work: &mut Cooperative,
    context: &RunContext,
) -> Result<Value, Error> {
    let column = match measure {
        Reduction::Count => {
            return Ok(Value::Int64(
                i64::try_from(rows.len()).map_err(|_| invalid("count overflow"))?,
            ))
        }
        Reduction::Sum(i) | Reduction::Avg(i) | Reduction::Min(i) | Reduction::Max(i) => *i,
        Reduction::Cardinality(columns) => {
            let mut workspace = Workspace::new(context)?;
            let mut identities = std::collections::BTreeSet::new();
            for row in rows {
                work.checkpoint().await?;
                if columns
                    .iter()
                    .any(|column| matches!(row[*column], Value::Null))
                {
                    continue;
                }
                let key = group_key(row, columns)?;
                if !identities.contains(&key) {
                    workspace.grow(key_bytes(&key))?;
                    identities.insert(key);
                }
            }
            return Ok(Value::Int64(
                i64::try_from(identities.len()).map_err(|_| invalid("distinct count overflow"))?,
            ));
        }
        Reduction::FrequencyL2(column) | Reduction::FrequencyEntropy(column) => {
            let mut workspace = Workspace::new(context)?;
            let mut counts = BTreeMap::<Vec<u8>, u64>::new();
            let mut total = 0_u64;
            for row in rows {
                work.checkpoint().await?;
                let value = &row[*column];
                if matches!(value, Value::Null) {
                    continue;
                }
                if matches!(value, Value::Float64(v) if !v.is_finite()) {
                    return Err(invalid(
                        "frequency aggregate requires a finite floating identity",
                    ));
                }
                let key = value.key()?;
                if !counts.contains_key(&key) {
                    workspace.grow(
                        64 + std::mem::size_of::<Vec<u8>>()
                            + key.len()
                            + std::mem::size_of::<u64>(),
                    )?;
                }
                *counts.entry(key).or_default() += 1;
                total += 1;
            }
            let mut result = 0.0_f64;
            for count in counts.into_values() {
                work.checkpoint().await?;
                let count = count as f64;
                if matches!(measure, Reduction::FrequencyL2(_)) {
                    result = result.hypot(count);
                } else {
                    let probability = count / total as f64;
                    result -= probability * probability.log2();
                }
            }
            return Ok(Value::Float64(result));
        }
        Reduction::Quantile { column, .. } | Reduction::Percentile { column, .. } => {
            let mut values = Vec::with_capacity(rows.len());
            for row in rows {
                work.checkpoint().await?;
                match &row[*column] {
                    Value::Float64(value) => values.push(*value),
                    Value::Null => {}
                    _ => return Err(invalid("floating quantile value required")),
                }
            }
            return Ok(match measure {
                Reduction::Percentile { q, .. } => {
                    super::percentile_cont(*q, values).map_or(Value::Null, Value::Float64)
                }
                Reduction::Quantile { q, .. } => Value::Float64(quantile(*q, values)),
                _ => unreachable!(),
            });
        }
    };
    let values = rows
        .iter()
        .map(|r| &r[column])
        .filter(|v| !matches!(v, Value::Null));
    if matches!(measure, Reduction::Min(_) | Reduction::Max(_)) {
        if plain(input, column)?.0 == &DataType::Float64 {
            // Match exact-state kernels: ignore NaN when a numeric value exists.
            let mut best: Option<f64> = None;
            for value in values {
                work.checkpoint().await?;
                let Value::Float64(value) = value else {
                    return Err(invalid("floating aggregate value required"));
                };
                best = Some(best.map_or(*value, |old| {
                    if matches!(measure, Reduction::Min(_)) {
                        old.min(*value)
                    } else {
                        old.max(*value)
                    }
                }));
            }
            return Ok(best.map(Value::Float64).unwrap_or(Value::Null));
        }
        let mut best: Option<&Value> = None;
        for value in values {
            work.checkpoint().await?;
            if best
                .map(|b| value.compare(b))
                .transpose()?
                .is_none_or(|order| {
                    if matches!(measure, Reduction::Min(_)) {
                        order.is_lt()
                    } else {
                        order.is_gt()
                    }
                })
            {
                best = Some(value);
            }
        }
        return Ok(best.cloned().unwrap_or(Value::Null));
    }
    let dtype = plain(input, column)?.0;
    if dtype == &DataType::Int64 {
        let mut count = 0usize;
        let mut sum = 0i128;
        for v in values {
            work.checkpoint().await?;
            let Value::Int64(v) = v else {
                return Err(invalid("integer aggregate value required"));
            };
            sum = sum
                .checked_add(i128::from(*v))
                .ok_or_else(|| invalid("integer aggregate overflow"))?;
            count += 1;
        }
        if count == 0 && !input.has_promql_series_identity() {
            return Ok(Value::Null);
        }
        return if matches!(measure, Reduction::Avg(_)) {
            Ok(Value::Float64(sum as f64 / count as f64))
        } else {
            Ok(Value::Int64(
                i64::try_from(sum).map_err(|_| invalid("integer sum overflow"))?,
            ))
        };
    }
    let mut floats = Vec::with_capacity(rows.len());
    for v in values {
        work.checkpoint().await?;
        let Value::Float64(v) = v else {
            return Err(invalid("floating aggregate value required"));
        };
        floats.push(*v);
    }
    if floats.is_empty() && !input.has_promql_series_identity() {
        return Ok(Value::Null);
    }
    Ok(Value::Float64(if matches!(measure, Reduction::Avg(_)) {
        promql_avg(&floats)
    } else {
        // Prometheus starts `sum` from the first value; adding it to 0 gives
        // the same sum and compensation.
        promql_sum(0., &floats)
    }))
}

mod tests {
    use super::*;
    use crate::plan::PhysicalDAG;
    use crate::runtime::{Limits, Scope};
    use planner_types::ir::schema::{Field, Schema, PROMQL_SERIES_IDENTITY};

    fn field(name: &str, dtype: DataType, nullable: bool) -> Field {
        Field {
            name: name.into(),
            dtype: SummaryFamilyType::Plain(dtype),
            nullable,
            table: None,
        }
    }

    /// Columns: 0 text?, 1 int?, 2 float?, 3 bool?, 4 time, 5 float (no
    /// nulls), 6 always NULL; with `promql`, 7 the series identity.
    fn input(promql: bool) -> SchemaRef {
        let mut fields = vec![
            field("text", DataType::Utf8, true),
            field("int", DataType::Int64, true),
            field("float", DataType::Float64, true),
            field("flag", DataType::Bool, true),
            field("time", DataType::Timestamp, false),
            field("value", DataType::Float64, false),
            field("nothing", DataType::Null, true),
        ];
        if promql {
            fields.push(field(PROMQL_SERIES_IDENTITY, DataType::Utf8, false));
        }
        Arc::new(Schema {
            fields,
            time_index: None,
            unique_keys: vec![],
            closed: false,
        })
    }

    fn batches(schema: &SchemaRef, seed: u64) -> Vec<Batch> {
        let mut seed = seed;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        let floats = [
            0.0,
            -0.0,
            0.1,
            f64::NAN,
            1e16,
            -1e16,
            1e308,
            1.0,
            f64::INFINITY,
            3.25,
        ];
        (0..3)
            .map(|_| {
                let rows = (0..120)
                    .map(|_| {
                        let maybe =
                            |n: usize, v: Value| if n.is_multiple_of(6) { Value::Null } else { v };
                        let mut row = vec![
                            maybe(next(), Value::Utf8(["a", "b", "c", ""][next() % 4].into())),
                            maybe(next(), Value::Int64((next() % 5) as i64 - 2)),
                            maybe(next(), Value::Float64(floats[next() % 10])),
                            maybe(next(), Value::Bool(next().is_multiple_of(2))),
                            Value::Timestamp((next() % 7) as i64),
                            Value::Float64(floats[next() % 10]),
                            Value::Null,
                        ];
                        if schema.fields.len() == 8 {
                            row.push(Value::Utf8(format!("{{\"s\":\"{}\"}}", next() % 3).into()));
                        }
                        row
                    })
                    .collect();
                Batch::try_new(schema.clone(), rows).unwrap()
            })
            .collect()
    }

    fn run(operator: Operator, input: Vec<Batch>) -> Result<Vec<Vec<Value>>, Error> {
        let mut dag = PhysicalDAG::default();
        let schema = operator.input_schemas()[0].clone();
        dag.add(0, vec![], Operator::source(schema, input).unwrap())
            .unwrap();
        dag.add(1, vec![0], operator).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
            },
            Limits::default(),
        )
        .unwrap();
        futures::executor::block_on(async {
            let mut output = dag.execute(&[1], context).unwrap().remove(0);
            let mut rows = Vec::new();
            while let Some(batch) = output.next().await {
                rows.extend(batch?.to_rows().iter().cloned());
            }
            Ok(rows)
        })
    }

    fn same(a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(a, b)| {
                a.iter().zip(b).all(|(a, b)| match (a, b) {
                    (Value::Float64(a), Value::Float64(b)) => a.to_bits() == b.to_bits(),
                    _ => format!("{a:?}") == format!("{b:?}"),
                })
            })
    }

    // The columnar Aggregate gives the row-at-a-time reference's groups,
    // labels, order and values (float bits included), for every reduction,
    // with and without per-measure filters, over several batches.
    #[test]
    fn columnar_aggregate_equals_the_row_reference() {
        use crate::plan::PhysicalOperator;
        let measures = |value: usize| {
            vec![
                ("count".to_string(), Reduction::Count),
                ("distinct".to_string(), Reduction::Cardinality(vec![0, 1])),
                ("l2".to_string(), Reduction::FrequencyL2(0)),
                ("entropy".to_string(), Reduction::FrequencyEntropy(1)),
                ("sum".to_string(), Reduction::Sum(value)),
                ("avg".to_string(), Reduction::Avg(value)),
                ("min".to_string(), Reduction::Min(value)),
                ("max".to_string(), Reduction::Max(value)),
                ("int_sum".to_string(), Reduction::Sum(1)),
                ("int_avg".to_string(), Reduction::Avg(1)),
                ("text_min".to_string(), Reduction::Min(0)),
                ("time_max".to_string(), Reduction::Max(4)),
                ("q".to_string(), Reduction::Quantile { column: 5, q: 0.75 }),
                // A NULL-typed column has no value to count.
                ("no_distinct".to_string(), Reduction::Cardinality(vec![6])),
                ("no_pair".to_string(), Reduction::Cardinality(vec![0, 6])),
            ]
        };
        for promql in [false, true] {
            let schema = input(promql);
            for groups in [vec![], vec![0], vec![2], vec![0, 1], vec![3, 2, 0]] {
                for value in [2, 5] {
                    for filtered in [false, true] {
                        if filtered && promql {
                            continue;
                        }
                        let mut operator =
                            Operator::aggregate(schema.clone(), groups.clone(), measures(value))
                                .unwrap();
                        let filters = (0..measures(value).len())
                            .map(|i| {
                                (filtered && i % 2 == 0).then(|| {
                                    Expression::IsNull(Box::new(Expression::Column(i % 4)))
                                })
                            })
                            .collect::<Vec<_>>();
                        if filtered {
                            operator = operator.with_measure_filters(filters.clone()).unwrap();
                        }
                        for (seed, input) in [(5, batches(&schema, 5)), (9, vec![])] {
                            let rows = input
                                .iter()
                                .flat_map(|batch| batch.to_rows().to_vec())
                                .collect::<Vec<_>>();
                            let context = RunContext::new(
                                Scope::Query {
                                    evaluation_time_ms: 0,
                                },
                                Limits::default(),
                            )
                            .unwrap();
                            let filters = if filtered { filters.clone() } else { vec![] };
                            let measures = measures(value)
                                .into_iter()
                                .map(|(_, measure)| measure)
                                .collect::<Vec<_>>();
                            let expected = futures::executor::block_on(reduce(
                                rows,
                                &groups,
                                &measures,
                                &filters,
                                &operator.input_schemas()[0],
                                &context,
                            ));
                            let input = if input.is_empty() {
                                vec![Batch::try_new(schema.clone(), vec![]).unwrap()]
                            } else {
                                input
                            };
                            let actual = run(operator.clone(), input);
                            let case = format!("promql {promql} groups {groups:?} value {value} filtered {filtered} seed {seed}");
                            match (expected, actual) {
                                (Ok(expected), Ok(actual)) => {
                                    assert!(
                                        same(&expected, &actual),
                                        "{case}\n{expected:?}\n{actual:?}"
                                    )
                                }
                                (Err(e), Err(a)) => {
                                    assert_eq!(e.to_string(), a.to_string(), "{case}")
                                }
                                (e, a) => panic!("{case}: reference {e:?}, columnar {a:?}"),
                            }
                        }
                    }
                }
            }
        }
    }
}
