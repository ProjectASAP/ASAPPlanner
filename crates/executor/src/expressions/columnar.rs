//! Expressions evaluated over a whole batch, one result column per call.
//!
//! An expression is evaluated one of two ways, chosen from its structure
//! and the input schema, with the same result as evaluating it row by row:
//!
//! - **Vectorized:** an expression built only from operations that cannot
//!   fail on well-typed input (column reads, literals, comparisons of
//!   scalar types, Kleene logic, null tests, `Float64` arithmetic and
//!   negation, `Int64`-to-`Float64` casts) runs as Arrow kernels or tight
//!   loops over whole columns.
//! - **Per distinct input:** any other expression runs through its row
//!   semantics. When every column it reads is a dictionary-encoded text
//!   column (PromQL labels and series identities), it runs once per
//!   distinct combination of dictionary keys, in order of first appearance,
//!   so a label matcher is evaluated once per series, not once per sample.
//!   Otherwise it runs once per row over the decoded columns it reads.
//!
//! Errors are those of row-by-row evaluation: vectorized operations cannot
//! fail, and the per-input path visits inputs in row order.
use super::{arithmetic, Expression};
use crate::values::arrow_columns::{self, ColumnDepth};
use crate::values::{Batch, Value};
use crate::Error;
use arrow::array::{
    Array, ArrayRef, BooleanArray, Datum, DictionaryArray, Float64Array, Int64Array, Scalar,
    UInt32Array,
};
use arrow::datatypes::UInt32Type;
use planner_types::ir::scalar::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use planner_types::ir::schema::{DataType, FieldDataType, Schema};
use planner_types::ir::ScalarExpr;
use std::{collections::HashMap, sync::Arc};

/// An expression lowered to the operations evaluated over whole columns.
enum VectorExpr {
    /// A batch column, shared without copying.
    Column(usize),
    /// One value for every row.
    Literal(Value, DataType),
    /// `-x` over `Float64`.
    NegateFloat(Box<VectorExpr>),
    /// `x op y` over `Float64`, with the executor's IEEE semantics.
    FloatArithmetic(ArithmeticOpKind, Box<VectorExpr>, Box<VectorExpr>),
    /// A comparison of two operands of one scalar type (the `DataType`).
    Compare(CompareOpKind, Box<VectorExpr>, Box<VectorExpr>, DataType),
    /// Kleene conjunction; empty is true.
    And(Vec<VectorExpr>),
    /// Kleene disjunction; empty is false.
    Or(Vec<VectorExpr>),
    /// Kleene negation.
    Not(Box<VectorExpr>),
    /// Whether the operand is null.
    IsNull(Box<VectorExpr>),
    /// Whether the operand is not null.
    IsNotNull(Box<VectorExpr>),
    /// `Int64` to `Float64`, as `i64 as f64`.
    IntToFloat(Box<VectorExpr>),
}

/// The result of a vectorized operation: a full column, or one value for
/// every row (a literal not yet expanded).
enum Evaluated {
    Column(ArrayRef),
    Constant(Value, DataType),
}

fn plain_type(schema: &Schema, column: usize) -> Option<&DataType> {
    match &schema.fields.get(column)?.dtype {
        FieldDataType::Plain(dtype) => Some(dtype),
        _ => None,
    }
}

fn scalar_comparable(dtype: &DataType) -> bool {
    matches!(
        dtype,
        DataType::Int64
            | DataType::Float64
            | DataType::Utf8
            | DataType::Bool
            | DataType::Timestamp
            | DataType::Date
    )
}

/// `expression` as vectorized operations with its result type, or `None`
/// when some part of it can fail or has no vectorized form.
fn lower_expression(expression: &Expression, schema: &Schema) -> Option<(VectorExpr, DataType)> {
    use Expression as E;
    Some(match expression {
        E::Column(column) => (
            VectorExpr::Column(*column),
            plain_type(schema, *column)?.clone(),
        ),
        E::Literal { value, dtype } => (
            VectorExpr::Literal(value.clone(), dtype.clone()),
            dtype.clone(),
        ),
        E::ExactFloat64(column) if plain_type(schema, *column)? == &DataType::Float64 => {
            (VectorExpr::Column(*column), DataType::Float64)
        }
        E::Negate(operand) => {
            let (operand, dtype) = lower_expression(operand, schema)?;
            if dtype != DataType::Float64 {
                return None;
            }
            (VectorExpr::NegateFloat(Box::new(operand)), dtype)
        }
        E::Arithmetic { op, left, right } => {
            let (left, a) = lower_expression(left, schema)?;
            let (right, b) = lower_expression(right, schema)?;
            if a != DataType::Float64 || b != DataType::Float64 {
                return None;
            }
            (
                VectorExpr::FloatArithmetic(op.clone(), Box::new(left), Box::new(right)),
                DataType::Float64,
            )
        }
        E::Equal(left, right) | E::Less(left, right) => {
            let (l, a) = lower_expression(left, schema)?;
            let (r, b) = lower_expression(right, schema)?;
            if a != b || !scalar_comparable(&a) {
                return None;
            }
            let op = if matches!(expression, E::Equal(..)) {
                CompareOpKind::Eq
            } else {
                CompareOpKind::Lt
            };
            (
                VectorExpr::Compare(op, Box::new(l), Box::new(r), a),
                DataType::Bool,
            )
        }
        E::And(left, right) | E::Or(left, right) => {
            let (l, a) = lower_expression(left, schema)?;
            let (r, b) = lower_expression(right, schema)?;
            if a != DataType::Bool || b != DataType::Bool {
                return None;
            }
            let parts = vec![l, r];
            (
                if matches!(expression, E::And(..)) {
                    VectorExpr::And(parts)
                } else {
                    VectorExpr::Or(parts)
                },
                DataType::Bool,
            )
        }
        E::Not(operand) => {
            let (operand, dtype) = lower_expression(operand, schema)?;
            if dtype != DataType::Bool {
                return None;
            }
            (VectorExpr::Not(Box::new(operand)), DataType::Bool)
        }
        E::IsNull(operand) => {
            let (operand, _) = lower_expression(operand, schema)?;
            (VectorExpr::IsNull(Box::new(operand)), DataType::Bool)
        }
        E::Planner(compiled) => lower_scalar(compiled.expression(), schema)?,
        _ => return None,
    })
}

fn literal(value: &ScalarValue) -> (Value, DataType) {
    match value {
        ScalarValue::Null => (Value::Null, DataType::Null),
        ScalarValue::Int64(v) => (Value::Int64(*v), DataType::Int64),
        ScalarValue::Float64(v) => (Value::Float64(*v), DataType::Float64),
        ScalarValue::Utf8(v) => (Value::Utf8(v.as_str().into()), DataType::Utf8),
        ScalarValue::Boolean(v) => (Value::Bool(*v), DataType::Bool),
        ScalarValue::Interval {
            months,
            days,
            nanos,
        } => (
            Value::Interval {
                months: *months,
                days: *days,
                nanos: *nanos,
            },
            DataType::Interval,
        ),
    }
}

/// [`lower_expression`] for a Planner scalar expression.
fn lower_scalar(expression: &ScalarExpr, schema: &Schema) -> Option<(VectorExpr, DataType)> {
    Some(match expression {
        ScalarExpr::Column(column) => (
            VectorExpr::Column(*column),
            plain_type(schema, *column)?.clone(),
        ),
        ScalarExpr::Literal(value) => {
            let (value, dtype) = literal(value);
            (VectorExpr::Literal(value, dtype.clone()), dtype)
        }
        ScalarExpr::Negative { expr, .. } => {
            let (operand, dtype) = lower_scalar(expr, schema)?;
            if dtype != DataType::Float64 {
                return None;
            }
            (VectorExpr::NegateFloat(Box::new(operand)), dtype)
        }
        ScalarExpr::Arithmetic {
            op, left, right, ..
        } => {
            let (left, a) = lower_scalar(left, schema)?;
            let (right, b) = lower_scalar(right, schema)?;
            if a != DataType::Float64 || b != DataType::Float64 {
                return None;
            }
            (
                VectorExpr::FloatArithmetic(op.clone(), Box::new(left), Box::new(right)),
                DataType::Float64,
            )
        }
        ScalarExpr::Compare {
            left, op, right, ..
        } => {
            if !matches!(
                op,
                CompareOpKind::Eq
                    | CompareOpKind::Ne
                    | CompareOpKind::Lt
                    | CompareOpKind::Le
                    | CompareOpKind::Gt
                    | CompareOpKind::Ge
            ) {
                return None;
            }
            let (l, a) = lower_scalar(left, schema)?;
            let (r, b) = lower_scalar(right, schema)?;
            // Dates have no Planner comparison; Int64 against Float64 has
            // its own exact semantics on the row path.
            if a != b || !scalar_comparable(&a) || a == DataType::Date {
                return None;
            }
            (
                VectorExpr::Compare(op.clone(), Box::new(l), Box::new(r), a),
                DataType::Bool,
            )
        }
        ScalarExpr::BoolAnd(parts) | ScalarExpr::BoolOr(parts) => {
            let lowered = parts
                .iter()
                .map(|part| {
                    let (part, dtype) = lower_scalar(part, schema)?;
                    (dtype == DataType::Bool).then_some(part)
                })
                .collect::<Option<Vec<_>>>()?;
            (
                if matches!(expression, ScalarExpr::BoolAnd(_)) {
                    VectorExpr::And(lowered)
                } else {
                    VectorExpr::Or(lowered)
                },
                DataType::Bool,
            )
        }
        ScalarExpr::Not(operand) => {
            let (operand, dtype) = lower_scalar(operand, schema)?;
            if dtype != DataType::Bool {
                return None;
            }
            (VectorExpr::Not(Box::new(operand)), DataType::Bool)
        }
        ScalarExpr::IsNull(operand) => {
            let (operand, _) = lower_scalar(operand, schema)?;
            (VectorExpr::IsNull(Box::new(operand)), DataType::Bool)
        }
        ScalarExpr::IsNotNull(operand) => {
            let (operand, _) = lower_scalar(operand, schema)?;
            (VectorExpr::IsNotNull(Box::new(operand)), DataType::Bool)
        }
        ScalarExpr::Cast { expr, to, .. } => {
            let (operand, dtype) = lower_scalar(expr, schema)?;
            match (&dtype, to) {
                (from, to) if from == to => (operand, dtype),
                (DataType::Int64, DataType::Float64) => {
                    (VectorExpr::IntToFloat(Box::new(operand)), DataType::Float64)
                }
                _ => return None,
            }
        }
        _ => return None,
    })
}

impl Evaluated {
    fn into_array(self, rows: usize) -> Result<ArrayRef, Error> {
        match self {
            Self::Column(array) => Ok(array),
            Self::Constant(value, dtype) => {
                arrow_columns::repeat(&dtype, ColumnDepth::BatchColumn, &value, rows)
            }
        }
    }
    fn boolean(self, rows: usize) -> Result<BooleanArray, Error> {
        Ok(self
            .into_array(rows)?
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("boolean operands are boolean arrays")
            .clone())
    }
}

fn arrow_error(error: arrow::error::ArrowError) -> Error {
    Error::Invalid(format!("columnar expression: {error}"))
}

/// A float operand: a column or one value for all rows.
enum FloatOperand {
    Column(Float64Array),
    Constant(Option<f64>),
}
impl FloatOperand {
    fn of(value: Evaluated) -> Self {
        match value {
            Evaluated::Column(array) => Self::Column(
                array
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .expect("Float64 operands are Float64 arrays")
                    .clone(),
            ),
            Evaluated::Constant(Value::Float64(v), _) => Self::Constant(Some(v)),
            Evaluated::Constant(_, _) => Self::Constant(None),
        }
    }
    fn get(&self, row: usize) -> Option<f64> {
        match self {
            Self::Column(array) => (!array.is_null(row)).then(|| array.value(row)),
            Self::Constant(value) => *value,
        }
    }
}

fn float_compare(op: &CompareOpKind, a: f64, b: f64) -> bool {
    // IEEE comparisons: NaN compares false except `!=`, and -0 == +0, as
    // the row semantics (`compare`, `Value::compare`) define.
    match op {
        CompareOpKind::Eq => a == b,
        CompareOpKind::Ne => a != b,
        CompareOpKind::Lt => a < b,
        CompareOpKind::Le => a <= b,
        CompareOpKind::Gt => a > b,
        CompareOpKind::Ge => a >= b,
        _ => unreachable!("lowered comparisons are ordering comparisons"),
    }
}

fn datum(value: &Evaluated) -> Result<Box<dyn Datum>, Error> {
    Ok(match value {
        Evaluated::Column(array) => Box::new(array.clone()),
        Evaluated::Constant(value, dtype) => Box::new(Scalar::new(arrow_columns::repeat(
            dtype,
            ColumnDepth::BatchColumn,
            value,
            1,
        )?)),
    })
}

fn evaluate_vector(expression: &VectorExpr, batch: &Batch) -> Result<Evaluated, Error> {
    let rows = batch.num_rows();
    Ok(match expression {
        VectorExpr::Column(column) => Evaluated::Column(batch.column(*column).clone()),
        VectorExpr::Literal(value, dtype) => Evaluated::Constant(value.clone(), dtype.clone()),
        VectorExpr::NegateFloat(operand) => match evaluate_vector(operand, batch)? {
            Evaluated::Constant(Value::Float64(v), dtype) => {
                Evaluated::Constant(Value::Float64(-v), dtype)
            }
            constant @ Evaluated::Constant(..) => constant,
            Evaluated::Column(array) => Evaluated::Column(Arc::new(
                array
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .expect("Float64 operand")
                    .unary::<_, arrow::datatypes::Float64Type>(|v| -v),
            )),
        },
        VectorExpr::FloatArithmetic(op, left, right) => {
            let left = FloatOperand::of(evaluate_vector(left, batch)?);
            let right = FloatOperand::of(evaluate_vector(right, batch)?);
            let result: Float64Array = (0..rows)
                .map(|row| match (left.get(row), right.get(row)) {
                    (Some(a), Some(b)) => Some(arithmetic::evaluate_float64_arithmetic(op, a, b)),
                    _ => None,
                })
                .collect();
            Evaluated::Column(Arc::new(result))
        }
        VectorExpr::Compare(op, left, right, DataType::Float64) => {
            let left = FloatOperand::of(evaluate_vector(left, batch)?);
            let right = FloatOperand::of(evaluate_vector(right, batch)?);
            let result: BooleanArray = (0..rows)
                .map(|row| match (left.get(row), right.get(row)) {
                    (Some(a), Some(b)) => Some(float_compare(op, a, b)),
                    _ => None,
                })
                .collect();
            Evaluated::Column(Arc::new(result))
        }
        VectorExpr::Compare(op, left, right, _) => {
            let left = evaluate_vector(left, batch)?;
            let right = evaluate_vector(right, batch)?;
            if let (Evaluated::Constant(..), Evaluated::Constant(..)) = (&left, &right) {
                // Arrow compares two scalars as a scalar; expand one side.
                let left = Evaluated::Column(left.into_array(rows)?);
                return compare_kernel(op, &left, &right);
            }
            return compare_kernel(op, &left, &right);
        }
        VectorExpr::And(parts) | VectorExpr::Or(parts) => {
            let and = matches!(expression, VectorExpr::And(_));
            let mut result: Option<BooleanArray> = None;
            for part in parts {
                let part = evaluate_vector(part, batch)?.boolean(rows)?;
                result = Some(match result {
                    None => part,
                    Some(previous) if and => {
                        arrow::compute::and_kleene(&previous, &part).map_err(arrow_error)?
                    }
                    Some(previous) => {
                        arrow::compute::or_kleene(&previous, &part).map_err(arrow_error)?
                    }
                });
            }
            match result {
                Some(result) => Evaluated::Column(Arc::new(result)),
                None => Evaluated::Constant(Value::Bool(and), DataType::Bool),
            }
        }
        VectorExpr::Not(operand) => {
            let operand = evaluate_vector(operand, batch)?.boolean(rows)?;
            Evaluated::Column(Arc::new(
                arrow::compute::not(&operand).map_err(arrow_error)?,
            ))
        }
        VectorExpr::IsNull(operand) | VectorExpr::IsNotNull(operand) => {
            let null = matches!(expression, VectorExpr::IsNull(_));
            let operand = evaluate_vector(operand, batch)?.into_array(rows)?;
            let result = if null {
                arrow::compute::is_null(operand.as_ref())
            } else {
                arrow::compute::is_not_null(operand.as_ref())
            }
            .map_err(arrow_error)?;
            Evaluated::Column(Arc::new(result))
        }
        VectorExpr::IntToFloat(operand) => match evaluate_vector(operand, batch)? {
            Evaluated::Constant(Value::Int64(v), _) => {
                Evaluated::Constant(Value::Float64(v as f64), DataType::Float64)
            }
            Evaluated::Constant(value, _) => Evaluated::Constant(value, DataType::Float64),
            Evaluated::Column(array) => Evaluated::Column(Arc::new(
                array
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .expect("Int64 operand")
                    .unary::<_, arrow::datatypes::Float64Type>(|v| v as f64),
            )),
        },
    })
}

fn compare_kernel(
    op: &CompareOpKind,
    left: &Evaluated,
    right: &Evaluated,
) -> Result<Evaluated, Error> {
    use arrow::compute::kernels::cmp;
    let (l, r) = (datum(left)?, datum(right)?);
    let (l, r) = (l.as_ref(), r.as_ref());
    let result = match op {
        CompareOpKind::Eq => cmp::eq(l, r),
        CompareOpKind::Ne => cmp::neq(l, r),
        CompareOpKind::Lt => cmp::lt(l, r),
        CompareOpKind::Le => cmp::lt_eq(l, r),
        CompareOpKind::Gt => cmp::gt(l, r),
        CompareOpKind::Ge => cmp::gt_eq(l, r),
        _ => unreachable!("lowered comparisons are ordering comparisons"),
    }
    .map_err(arrow_error)?;
    Ok(Evaluated::Column(Arc::new(result)))
}

/// A well-typed non-null value of `dtype`, filling row cells an expression
/// does not read.
fn placeholder(dtype: &DataType) -> Value {
    match dtype {
        DataType::Null => Value::Null,
        DataType::Bool => Value::Bool(false),
        DataType::Int64 => Value::Int64(0),
        DataType::Float64 => Value::Float64(0.),
        DataType::Utf8 => Value::Utf8("".into()),
        DataType::Timestamp => Value::Timestamp(0),
        DataType::Date => Value::Date(0),
        DataType::Interval => Value::Interval {
            months: 0,
            days: 0,
            nanos: 0,
        },
        DataType::List { .. } => Value::List(Arc::from([])),
        DataType::Struct { fields } => {
            Value::Struct(fields.iter().map(|f| placeholder(&f.dtype)).collect())
        }
        DataType::Map { .. } => Value::Map(Arc::from([])),
    }
}

/// Evaluate `evaluate_row` for each row of `batch`, giving it a full-width
/// row in which only `columns` hold the row's values. The results form a
/// column of `dtype`.
fn evaluate_per_input(
    batch: &Batch,
    columns: &[usize],
    dtype: &DataType,
    mut evaluate_row: impl FnMut(&[Value]) -> Result<Value, Error>,
) -> Result<ArrayRef, Error> {
    let rows = batch.num_rows();
    let schema = batch.schema();
    let mut row = schema
        .fields
        .iter()
        .map(|field| match &field.dtype {
            FieldDataType::Plain(dtype) => placeholder(dtype),
            _ => Value::Null,
        })
        .collect::<Vec<_>>();
    let dictionaries = columns
        .iter()
        .map(|&column| {
            batch
                .column(column)
                .as_any()
                .downcast_ref::<DictionaryArray<UInt32Type>>()
                .cloned()
        })
        .collect::<Option<Vec<_>>>();
    if let Some(dictionaries) = dictionaries {
        // Once per distinct combination of keys, in order of first appearance.
        let texts = dictionaries
            .iter()
            .map(arrow_columns::dictionary_texts)
            .collect::<Vec<_>>();
        let mut seen: HashMap<Vec<Option<u32>>, u32> = HashMap::new();
        let mut results: Vec<Value> = Vec::new();
        let mut indices = Vec::with_capacity(rows);
        let mut key = Vec::with_capacity(columns.len());
        for index in 0..rows {
            key.clear();
            key.extend(dictionaries.iter().map(|dictionary| {
                let keys = dictionary.keys();
                (!keys.is_null(index)).then(|| keys.value(index))
            }));
            let position = match seen.get(&key) {
                Some(&position) => position,
                None => {
                    for ((&column, code), texts) in columns.iter().zip(&key).zip(&texts) {
                        row[column] = match code {
                            Some(code) => Value::Utf8(texts[*code as usize].clone()),
                            None => Value::Null,
                        };
                    }
                    results.push(evaluate_row(&row)?);
                    let position = (results.len() - 1) as u32;
                    seen.insert(key.clone(), position);
                    position
                }
            };
            indices.push(position);
        }
        let distinct = arrow_columns::encode(
            dtype,
            ColumnDepth::BatchColumn,
            &results.iter().collect::<Vec<_>>(),
        )?;
        return arrow::compute::take(distinct.as_ref(), &UInt32Array::from(indices), None)
            .map_err(arrow_error);
    }
    let decoded = columns
        .iter()
        .map(|&column| {
            let FieldDataType::Plain(dtype) = &schema.fields[column].dtype else {
                return Err(Error::Invalid(
                    "scalar expression cannot consume summary state".into(),
                ));
            };
            Ok(arrow_columns::decode(
                batch.column(column).as_ref(),
                dtype,
                ColumnDepth::BatchColumn,
            ))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let mut results = Vec::with_capacity(rows);
    for index in 0..rows {
        for (&column, values) in columns.iter().zip(&decoded) {
            row[column] = values[index].clone();
        }
        results.push(evaluate_row(&row)?);
    }
    arrow_columns::encode(
        dtype,
        ColumnDepth::BatchColumn,
        &results.iter().collect::<Vec<_>>(),
    )
}

fn expression_columns(expression: &Expression, columns: &mut Vec<usize>) {
    use Expression as E;
    match expression {
        E::Column(c) | E::ExactFloat64(c) => columns.push(*c),
        E::LabelSet { column, .. } | E::Label { column, .. } | E::LabelIdentity { column, .. } => {
            columns.push(*column)
        }
        E::Planner(compiled) => columns.extend(compiled.expression().columns_referenced()),
        E::Literal { .. } => {}
        E::FiniteFloat64(e) | E::Negate(e) | E::Not(e) | E::IsNull(e) => {
            expression_columns(e, columns)
        }
        E::Binary { left, right, .. }
        | E::Arithmetic { left, right, .. }
        | E::Equal(left, right)
        | E::Less(left, right)
        | E::And(left, right)
        | E::Or(left, right) => {
            expression_columns(left, columns);
            expression_columns(right, columns);
        }
    }
}

fn distinct(mut columns: Vec<usize>) -> Vec<usize> {
    columns.sort_unstable();
    columns.dedup();
    columns
}

impl Expression {
    /// The column of this expression's value for every row of `batch`, of
    /// type `dtype` (the expression's type over the batch's schema). The
    /// same values and errors as [`Self::evaluate`] row by row.
    pub(crate) fn evaluate_batch(
        &self,
        batch: &Batch,
        dtype: &DataType,
    ) -> Result<ArrayRef, Error> {
        if let Some((vector, _)) = lower_expression(self, batch.schema()) {
            return evaluate_vector(&vector, batch)?.into_array(batch.num_rows());
        }
        let mut columns = Vec::new();
        expression_columns(self, &mut columns);
        evaluate_per_input(batch, &distinct(columns), dtype, |row| self.evaluate(row))
    }
}

impl super::CompiledExpression {
    /// The column of this expression's value for every row of `batch`,
    /// which must be in the schema it was compiled for. The same values and
    /// errors as [`Self::evaluate`] row by row.
    pub(crate) fn evaluate_batch(&self, batch: &Batch) -> Result<ArrayRef, Error> {
        if let Some((vector, _)) = lower_scalar(self.expression(), batch.schema()) {
            return evaluate_vector(&vector, batch)?.into_array(batch.num_rows());
        }
        let columns = distinct(self.expression().columns_referenced());
        evaluate_per_input(batch, &columns, &self.dtype().0, |row| self.evaluate(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expressions::{binary::BinaryOperator, CompiledExpression};
    use planner_types::ir::scalar::ExprSemantics;
    use planner_types::ir::schema::Field;

    fn field(name: &str, dtype: DataType, nullable: bool) -> Field {
        Field {
            name: name.into(),
            dtype: FieldDataType::Plain(dtype),
            nullable,
            table: None,
        }
    }

    fn labels_type() -> DataType {
        DataType::Map {
            key: Box::new(DataType::Utf8),
            value: Box::new(DataType::Utf8),
            value_nullable: false,
        }
    }

    // Columns: 0 f (Float64?), 1 g (Float64), 2 i (Int64?), 3 j (Int64),
    // 4 s (Utf8?), 5 t (Utf8), 6 b (Bool?), 7 ts (Timestamp), 8 labels.
    fn batch(rows: usize) -> Batch {
        let schema = Arc::new(Schema {
            fields: vec![
                field("f", DataType::Float64, true),
                field("g", DataType::Float64, false),
                field("i", DataType::Int64, true),
                field("j", DataType::Int64, false),
                field("s", DataType::Utf8, true),
                field("t", DataType::Utf8, false),
                field("b", DataType::Bool, true),
                field("ts", DataType::Timestamp, false),
                field("labels", labels_type(), false),
            ],
            time_index: None,
            unique_keys: vec![],
            closed: false,
        });
        let mut seed = 7u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        let floats = [0.0, -0.0, 1.5, -2.0, f64::NAN, f64::INFINITY, 3.0, 1e300];
        let ints = [0, 1, -1, 7, i64::MAX, i64::MIN, 1 << 53, 3];
        let texts = ["api", "web", "", "db", "api"];
        let rows = (0..rows)
            .map(|_| {
                let text = |n: u64| Value::Utf8(texts[n as usize % texts.len()].into());
                let maybe = |n: u64, v: Value| if n.is_multiple_of(5) { Value::Null } else { v };
                let job = text(next());
                vec![
                    maybe(next(), Value::Float64(floats[next() as usize % 8])),
                    Value::Float64(floats[next() as usize % 8]),
                    maybe(next(), Value::Int64(ints[next() as usize % 8])),
                    Value::Int64(ints[next() as usize % 8] % 1000),
                    maybe(next(), text(next())),
                    text(next()),
                    maybe(next(), Value::Bool(next().is_multiple_of(2))),
                    Value::Timestamp((next() % 5) as i64),
                    Value::Map(vec![(Value::Utf8("job".into()), job)].into()),
                ]
            })
            .collect();
        Batch::try_new(schema, rows).unwrap()
    }

    fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Float64(a), Value::Float64(b)) => a.to_bits() == b.to_bits(),
            _ => format!("{a:?}") == format!("{b:?}"),
        }
    }

    /// The batch result equals the row-by-row result, errors included.
    fn check(expression: &Expression, batch: &Batch) {
        let (dtype, _) = expression.dtype(batch.schema()).unwrap();
        let expected = batch
            .rows()
            .iter()
            .map(|row| expression.evaluate(row))
            .collect::<Result<Vec<_>, _>>();
        let actual = expression.evaluate_batch(batch, &dtype);
        match (expected, actual) {
            (Ok(expected), Ok(actual)) => {
                let actual =
                    arrow_columns::decode(actual.as_ref(), &dtype, ColumnDepth::BatchColumn);
                assert_eq!(expected.len(), actual.len(), "{expression:?}");
                for (e, a) in expected.iter().zip(&actual) {
                    assert!(same(e, a), "{expression:?}: {e:?} vs {a:?}");
                }
            }
            (Err(e), Err(a)) => assert_eq!(e.to_string(), a.to_string(), "{expression:?}"),
            (e, a) => panic!("{expression:?}: rows {e:?}, batch {a:?}"),
        }
    }

    fn planner(expression: ScalarExpr, batch: &Batch) -> Expression {
        Expression::planner(CompiledExpression::compile(&expression, batch.schema()).unwrap())
    }

    fn col(i: usize) -> Box<Expression> {
        Box::new(Expression::Column(i))
    }

    // Every evaluation path (vectorized, once per distinct dictionary key,
    // once per row) gives the row-by-row values and errors.
    #[test]
    fn batch_evaluation_equals_row_evaluation() {
        let batch = batch(300);
        use ArithmeticOpKind as A;
        use Expression as E;
        let mut expressions = vec![
            E::Column(0),
            E::Column(4),
            E::Column(8),
            E::Literal {
                value: Value::Utf8("api".into()),
                dtype: DataType::Utf8,
            },
            E::Literal {
                value: Value::Null,
                dtype: DataType::Float64,
            },
            E::Negate(col(0)),
            E::Negate(col(2)),
            E::ExactFloat64(1),
            E::ExactFloat64(3),
            E::FiniteFloat64(col(1)),
            E::Not(col(6)),
            E::IsNull(col(4)),
            E::And(col(6), Box::new(E::Less(col(0), col(1)))),
            E::Or(col(6), Box::new(E::Equal(col(4), col(5)))),
            E::Label {
                column: 8,
                name: "job".into(),
            },
            E::LabelIdentity {
                column: 8,
                excluding: vec![],
            },
            E::Binary {
                operator: BinaryOperator {
                    kind: crate::expressions::binary::BinaryOpKind::Arithmetic(A::Div),
                    vector_match: None,
                    checked_relative_division: true,
                    checked_finite_division: false,
                },
                left: col(0),
                right: col(1),
            },
        ];
        for op in [A::Add, A::Sub, A::Mul, A::Div, A::Mod, A::Pow, A::Atan2] {
            expressions.push(E::Arithmetic {
                op: op.clone(),
                left: col(0),
                right: col(1),
            });
            if op != A::Atan2 {
                expressions.push(E::Arithmetic {
                    op,
                    left: col(2),
                    right: col(3),
                });
            }
        }
        for (a, b) in [(0, 1), (2, 3), (4, 5), (6, 6), (7, 7)] {
            expressions.push(E::Equal(col(a), col(b)));
            expressions.push(E::Less(col(a), col(b)));
        }
        let compare =
            |left: ScalarExpr, op: CompareOpKind, right: ScalarExpr| ScalarExpr::Compare {
                left: Box::new(left),
                op,
                right: Box::new(right),
                semantics: ExprSemantics::Promql,
            };
        let api = || ScalarExpr::Literal(ScalarValue::Utf8("api".into()));
        for op in [
            CompareOpKind::Eq,
            CompareOpKind::Ne,
            CompareOpKind::Lt,
            CompareOpKind::Le,
            CompareOpKind::Gt,
            CompareOpKind::Ge,
        ] {
            for (a, b) in [(0, 1), (2, 3), (4, 5), (6, 6), (7, 7), (2, 0)] {
                expressions.push(planner(
                    compare(ScalarExpr::Column(a), op.clone(), ScalarExpr::Column(b)),
                    &batch,
                ));
            }
            expressions.push(planner(
                compare(ScalarExpr::Column(4), op.clone(), api()),
                &batch,
            ));
            expressions.push(planner(compare(api(), op, ScalarExpr::Column(5)), &batch));
        }
        expressions.extend([
            planner(
                ScalarExpr::BoolAnd(vec![
                    compare(ScalarExpr::Column(4), CompareOpKind::Eq, api()),
                    compare(ScalarExpr::Column(5), CompareOpKind::Ne, api()),
                    ScalarExpr::Column(6),
                ]),
                &batch,
            ),
            planner(
                ScalarExpr::BoolOr(vec![
                    ScalarExpr::Column(6),
                    ScalarExpr::IsNull(Box::new(ScalarExpr::Column(0))),
                ]),
                &batch,
            ),
            planner(ScalarExpr::BoolAnd(vec![]), &batch),
            planner(ScalarExpr::BoolOr(vec![]), &batch),
            planner(
                ScalarExpr::Not(Box::new(ScalarExpr::IsNotNull(Box::new(
                    ScalarExpr::Column(4),
                )))),
                &batch,
            ),
            planner(
                ScalarExpr::Cast {
                    expr: Box::new(ScalarExpr::Column(3)),
                    to: DataType::Float64,
                    try_cast: false,
                },
                &batch,
            ),
            planner(
                ScalarExpr::Arithmetic {
                    op: A::Mul,
                    left: Box::new(ScalarExpr::Column(0)),
                    right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(-2.0))),
                    semantics: ExprSemantics::Promql,
                },
                &batch,
            ),
            planner(
                ScalarExpr::Arithmetic {
                    op: A::Add,
                    left: Box::new(ScalarExpr::Column(2)),
                    right: Box::new(ScalarExpr::Column(1)),
                    semantics: ExprSemantics::Sql,
                },
                &batch,
            ),
            planner(
                ScalarExpr::Negative {
                    expr: Box::new(ScalarExpr::Column(1)),
                    semantics: ExprSemantics::Promql,
                },
                &batch,
            ),
            planner(
                ScalarExpr::FunctionCall {
                    name: "promql_abs".into(),
                    args: vec![ScalarExpr::Column(1)],
                },
                &batch,
            ),
            planner(
                ScalarExpr::Case {
                    operand: None,
                    branches: vec![(
                        compare(ScalarExpr::Column(5), CompareOpKind::Eq, api()),
                        ScalarExpr::Column(4),
                    )],
                    else_expr: Some(Box::new(api())),
                },
                &batch,
            ),
        ]);
        for expression in &expressions {
            check(expression, &batch);
            check(
                expression,
                &batch.filter(&BooleanArray::from(vec![false; 300])).unwrap(),
            );
        }
    }

    // A label matcher over dictionary-encoded labels runs once per distinct
    // label value, not once per row.
    #[test]
    fn dictionary_inputs_are_evaluated_once_per_distinct_value() {
        let batch = batch(300);
        let mut calls = 0;
        let result = evaluate_per_input(&batch, &[4, 5], &DataType::Bool, |row| {
            calls += 1;
            Ok(Value::Bool(
                matches!((&row[4], &row[5]), (Value::Utf8(a), Value::Utf8(b)) if a == b),
            ))
        })
        .unwrap();
        assert_eq!(result.len(), 300);
        // 4 distinct values (and null) by 4 distinct values at most.
        assert!(calls <= 20, "{calls} evaluations");
    }
}
