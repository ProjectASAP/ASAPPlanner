//! A column bound from another operator's whole output, as a scalar
//! expression reads one (the IR's `ScalarRef` edge: PromQL `scalar(v)`,
//! SQL subqueries): the reading expression then reads that column.
use super::*;

/// How a scalar expression reads another operator's output.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ScalarRefRead {
    /// PromQL `scalar(v)`: the `value` column (Float64, or Int64 for
    /// `count`) of `v`'s one sample, as a Float64; NaN unless `v` has
    /// exactly one.
    VectorScalar { value: usize },
    /// An SQL scalar subquery: its one row's one column; NULL without a
    /// row; more than one row is an error.
    Scalar,
    /// SQL `EXISTS`: whether the subquery has a row.
    Exists,
    /// SQL `probe IN (subquery)` per row, as `probe = v1 OR probe = v2 …`
    /// over the subquery's values: true on a match; otherwise NULL when the
    /// probe or a value is NULL, else false; false over no value.
    In { probe: Expression },
}

impl Operator {
    /// The rows of `input`, each with one more column, `name`, holding what
    /// `read` reads of `referenced`'s output.
    pub fn bind_scalar_ref(
        input: SchemaRef,
        referenced: SchemaRef,
        read: ScalarRefRead,
        name: &str,
    ) -> Result<Self, Error> {
        let one_column = || -> Result<&DataType, Error> {
            match referenced.fields.as_slice() {
                [_] => Ok(plain(&referenced, 0)?.0),
                _ => Err(invalid(
                    "an SQL subquery read needs a subquery of one column",
                )),
            }
        };
        let mut in_compares_floats = false;
        let (dtype, nullable) = match &read {
            ScalarRefRead::VectorScalar { value } => {
                if !matches!(
                    plain(&referenced, *value)?.0,
                    DataType::Float64 | DataType::Int64
                ) {
                    return Err(invalid("scalar() reads a Float64 or Int64 sample value"));
                }
                (DataType::Float64, false)
            }
            ScalarRefRead::Scalar => (one_column()?.clone(), true),
            ScalarRefRead::Exists => (DataType::Bool, false),
            ScalarRefRead::In { probe } => {
                let (probe, _) = probe.dtype(&input)?;
                let value = one_column()?;
                let numeric =
                    |dtype: &DataType| matches!(dtype, DataType::Int64 | DataType::Float64);
                if probe != *value && !(numeric(&probe) && numeric(value)) {
                    return Err(invalid("IN compares values of different types"));
                }
                // An Int64 against Float64 compares as a float.
                in_compares_floats = probe != *value;
                (DataType::Bool, true)
            }
        };
        let output = appended_schema(&input, name, dtype, nullable);
        Ok(Self {
            kind: Kind::BindScalarRef {
                read,
                in_compares_floats,
            },
            inputs: vec![input, referenced],
            output,
        })
    }
}

/// A value as `IN` compares it: an integer read against floats as a float.
fn in_key(value: &Value, as_float: bool) -> Result<Vec<u8>, Error> {
    match value {
        Value::Int64(v) if as_float => Value::Float64(*v as f64).key(),
        value => value.key(),
    }
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::BindScalarRef {
        read,
        in_compares_floats: as_float,
    } = &operator.kind
    else {
        unreachable!()
    };
    let as_float = *as_float;
    let mut subquery = inputs
        .pop()
        .ok_or_else(|| invalid("subquery input missing"))?;
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    let output = operator.output.clone();
    let bound = operator.output.fields.len() - 1;
    let SummaryFamilyType::Plain(dtype) = operator.output.fields[bound].dtype.clone() else {
        unreachable!("a bound column is plain")
    };
    let probe = match read {
        ScalarRefRead::In { probe } => Some(probe.at_evaluation_time(&context).into_owned()),
        _ => None,
    };
    let rows_context = context.clone();
    // The subquery's whole output, read once: its row count, the constant
    // it binds, and for `IN` its values' keys.
    let bound = async move {
        let mut work = Cooperative::new(&context);
        let mut workspace = Workspace::new(&context)?;
        let (mut rows, mut last) = (0usize, Value::Null);
        let mut keys = ahash::AHashSet::<Vec<u8>>::new();
        let mut has_null = false;
        while let Some(batch) = subquery.next().await {
            let batch = batch?;
            work.checkpoint_rows(batch.num_rows()).await?;
            rows = rows.saturating_add(batch.num_rows());
            let column = match read {
                ScalarRefRead::VectorScalar { value } => *value,
                ScalarRefRead::Exists => continue,
                ScalarRefRead::Scalar | ScalarRefRead::In { .. } => 0,
            };
            let values = decoded(&batch, column)?;
            if let ScalarRefRead::In { .. } = read {
                for value in &values {
                    match value {
                        Value::Null => has_null = true,
                        // NaN equals no value.
                        Value::Float64(v) if v.is_nan() => {}
                        value => {
                            let key = in_key(value, as_float)?;
                            workspace.grow(key.len() + 32)?;
                            keys.insert(key);
                        }
                    }
                }
            }
            if let Some(value) = values.into_iter().last() {
                last = value;
            }
        }
        let constant = match read {
            // `count`'s Int64 sample is its float.
            ScalarRefRead::VectorScalar { .. } if rows == 1 => match last {
                Value::Int64(v) => Value::Float64(v as f64),
                Value::Float64(v) => Value::Float64(v),
                _ => Value::Float64(f64::NAN),
            },
            ScalarRefRead::VectorScalar { .. } => Value::Float64(f64::NAN),
            ScalarRefRead::Scalar if rows > 1 => {
                return Err(Error::Operator(
                    "a scalar subquery returned more than one row".into(),
                ))
            }
            ScalarRefRead::Scalar => last,
            ScalarRefRead::Exists => Value::Bool(rows > 0),
            ScalarRefRead::In { .. } => Value::Null,
        };
        Ok(Bound {
            constant,
            keys,
            has_null,
            any_value: rows > 0,
            _workspace: workspace,
        })
    };
    let mut input = Some(input);
    Ok(futures::stream::once(bound)
        .flat_map(move |bound| {
            let input = input.take().expect("the subquery is read once");
            let bound = match bound {
                Ok(bound) => bound,
                Err(error) => return futures::stream::once(async { Err(error) }).boxed_local(),
            };
            let (output, probe, dtype, context) = (
                output.clone(),
                probe.clone(),
                dtype.clone(),
                rows_context.clone(),
            );
            input
                .map(move |batch| {
                    if context.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    let batch = batch?;
                    let column = match &probe {
                        None => crate::values::arrow_columns::repeat(
                            &dtype,
                            crate::values::arrow_columns::ColumnDepth::BatchColumn,
                            &bound.constant,
                            batch.num_rows(),
                        )?,
                        Some(probe) => bound.marks(probe, &batch, as_float)?,
                    };
                    batch.with_appended_column(output.clone(), column)
                })
                .boxed_local()
        })
        .boxed_local())
}

/// What a [`ScalarRefRead`] read of the subquery's whole output.
struct Bound {
    /// The bound value of every row, but for `IN`.
    constant: Value,
    /// For `IN`: the keys ([`in_key`]) of the subquery's non-null, non-NaN values.
    keys: ahash::AHashSet<Vec<u8>>,
    /// For `IN`: whether a subquery value is NULL.
    has_null: bool,
    /// Whether the subquery has a row.
    any_value: bool,
    /// The memory the keys hold.
    _workspace: Workspace,
}

impl Bound {
    /// `probe IN (subquery)` for each row of `batch`.
    fn marks(
        &self,
        probe: &Expression,
        batch: &Batch,
        as_float: bool,
    ) -> Result<arrow::array::ArrayRef, Error> {
        let (probe_type, _) = probe.dtype(batch.schema())?;
        let probed = probe.evaluate_batch(batch, &probe_type)?;
        let values = crate::values::arrow_columns::decode(
            probed.as_ref(),
            &probe_type,
            crate::values::arrow_columns::ColumnDepth::BatchColumn,
        );
        let unmatched = (!self.has_null).then_some(false);
        let marks = values
            .iter()
            .map(|value| {
                Ok(match value {
                    _ if !self.any_value => Some(false),
                    Value::Null => None,
                    Value::Float64(v) if v.is_nan() => unmatched,
                    value if self.keys.contains(&in_key(value, as_float)?) => Some(true),
                    _ => unmatched,
                })
            })
            .collect::<Result<arrow::array::BooleanArray, Error>>()?;
        Ok(Arc::new(marks))
    }
}
