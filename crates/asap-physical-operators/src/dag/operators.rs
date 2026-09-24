//! Native DAG operators. Engines bind sources; computation lives here.
use super::{
    values::{group_key, Batch, Schema, Value},
    Error, Input, OutputStream, PhysicalOperator, Reservation, RunContext,
};
use futures::StreamExt;
use planner_types::{
    post_asap::{SummaryFamilyType, SummaryField, SummarySchema, SummaryUpdate},
    pre_asap::{ArithmeticOpKind, ColumnRef, DataType},
};
use std::{collections::BTreeMap, sync::Arc};

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
fn field(schema: &Schema, column: usize) -> Result<&SummaryField, Error> {
    schema
        .fields
        .get(column)
        .ok_or_else(|| invalid("column out of range"))
}
fn plain(schema: &Schema, column: usize) -> Result<(&DataType, bool), Error> {
    let f = field(schema, column)?;
    let SummaryFamilyType::Plain(dtype) = &f.dtype else {
        return Err(invalid("plain value required"));
    };
    Ok((dtype, f.nullable))
}
fn schema(fields: Vec<SummaryField>) -> Schema {
    Arc::new(SummarySchema {
        fields,
        time_index: None,
    })
}
fn result_field(name: &str, dtype: DataType, nullable: bool) -> SummaryField {
    SummaryField {
        name: name.into(),
        dtype: SummaryFamilyType::Plain(dtype),
        nullable,
    }
}

#[derive(Clone, Debug)]
pub enum Expression {
    Binary {
        operator: planner_types::post_asap::BinaryOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Planner(Box<super::expressions::CompiledExpression>),
    Column(usize),
    Literal {
        value: Value,
        dtype: DataType,
    },
    Negate(Box<Expression>),
    Arithmetic {
        op: ArithmeticOpKind,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Equal(Box<Expression>, Box<Expression>),
    Less(Box<Expression>, Box<Expression>),
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),
    IsNull(Box<Expression>),
}
impl Expression {
    pub fn planner(expression: super::expressions::CompiledExpression) -> Self {
        Self::Planner(Box::new(expression))
    }
    fn dtype(&self, input: &Schema) -> Result<(DataType, bool), Error> {
        use Expression::*;
        match self {
            Binary {
                operator,
                left,
                right,
            } => {
                use planner_types::pre_asap::{BinaryOpKind, CompareOpKind};
                let (a, n) = left.dtype(input)?;
                let (b, m) = right.dtype(input)?;
                if a != DataType::Float64 || b != a || operator.vector_match.is_some() {
                    return Err(invalid(
                        "binary expression requires resolved Float64 operands",
                    ));
                }
                if (operator.checked_relative_division || operator.checked_finite_division)
                    && operator.kind != BinaryOpKind::Arithmetic(ArithmeticOpKind::Div)
                {
                    return Err(invalid("checked division contract on non-division"));
                }
                let dtype = match operator.kind {
                    BinaryOpKind::Arithmetic(_) => DataType::Float64,
                    BinaryOpKind::Compare(
                        CompareOpKind::Eq
                        | CompareOpKind::Ne
                        | CompareOpKind::Lt
                        | CompareOpKind::Le
                        | CompareOpKind::Gt
                        | CompareOpKind::Ge,
                    ) => DataType::Bool,
                    _ => return Err(invalid("unsupported binary operation")),
                };
                Ok((dtype, n || m))
            }
            Planner(expression) => Ok(expression.dtype()),
            Column(i) => {
                let (t, n) = plain(input, *i)?;
                Ok((t.clone(), n))
            }
            Literal { value, dtype } => {
                if value.matches(dtype, true) {
                    Ok((dtype.clone(), matches!(value, Value::Null)))
                } else {
                    Err(invalid("literal type mismatch"))
                }
            }
            Negate(v) => {
                let (t, n) = v.dtype(input)?;
                if matches!(t, DataType::Int64 | DataType::Float64) {
                    Ok((t, n))
                } else {
                    Err(invalid("numeric negation required"))
                }
            }
            Arithmetic { op, left, right } => {
                let (a, n) = left.dtype(input)?;
                let (b, m) = right.dtype(input)?;
                if a == b
                    && matches!(a, DataType::Int64 | DataType::Float64)
                    && !(a == DataType::Int64 && *op == ArithmeticOpKind::Atan2)
                {
                    Ok((a, n || m))
                } else {
                    Err(invalid("arithmetic requires matching numeric types"))
                }
            }
            Equal(a, b) | Less(a, b) => {
                let (a, n) = a.dtype(input)?;
                let (b, m) = b.dtype(input)?;
                if a == b && ordered(&a) {
                    Ok((DataType::Bool, n || m))
                } else {
                    Err(invalid("comparison requires matching ordered types"))
                }
            }
            And(a, b) | Or(a, b) => {
                let (a, n) = a.dtype(input)?;
                let (b, m) = b.dtype(input)?;
                if a == DataType::Bool && b == DataType::Bool {
                    Ok((DataType::Bool, n || m))
                } else {
                    Err(invalid("boolean operands required"))
                }
            }
            Not(v) => {
                let (t, n) = v.dtype(input)?;
                if t == DataType::Bool {
                    Ok((t, n))
                } else {
                    Err(invalid("boolean operand required"))
                }
            }
            IsNull(v) => {
                v.dtype(input)?;
                Ok((DataType::Bool, false))
            }
        }
    }
    fn evaluate(&self, row: &[Value]) -> Result<Value, Error> {
        use Expression::*;
        Ok(match self {
            Binary {
                operator,
                left,
                right,
            } => {
                let (a, b) = (left.evaluate(row)?, right.evaluate(row)?);
                if matches!(a, Value::Null) || matches!(b, Value::Null) {
                    Value::Null
                } else {
                    let (Value::Float64(a), Value::Float64(b)) = (a, b) else {
                        return Err(invalid("binary value schema mismatch"));
                    };
                    crate::arithmetic::evaluate_binary(operator, a, b)?
                }
            }
            Planner(expression) => expression.evaluate(row)?,
            Column(i) => row[*i].clone(),
            Literal { value, .. } => value.clone(),
            Negate(v) => match v.evaluate(row)? {
                Value::Int64(v) => Value::Int64(
                    v.checked_neg()
                        .ok_or_else(|| invalid("integer negation overflow"))?,
                ),
                Value::Float64(v) => Value::Float64(-v),
                Value::Null => Value::Null,
                _ => return Err(invalid("numeric negation required")),
            },
            Arithmetic { op, left, right } => {
                numeric(op, left.evaluate(row)?, right.evaluate(row)?)?
            }
            Equal(a, b) | Less(a, b) => {
                let (a, b) = (a.evaluate(row)?, b.evaluate(row)?);
                if matches!(a, Value::Null) || matches!(b, Value::Null) {
                    Value::Null
                } else if matches!((&a,&b),(Value::Float64(a),Value::Float64(b)) if a.is_nan() || b.is_nan())
                {
                    Value::Bool(false)
                } else {
                    let c = a.compare(&b)?;
                    Value::Bool(if matches!(self, Equal(..)) {
                        c.is_eq()
                    } else {
                        c.is_lt()
                    })
                }
            }
            And(a, b) | Or(a, b) => {
                let (a, b) = (a.evaluate(row)?, b.evaluate(row)?);
                match (a, b, matches!(self, And(..))) {
                    (Value::Bool(false), _, true) | (_, Value::Bool(false), true) => {
                        Value::Bool(false)
                    }
                    (Value::Bool(true), _, false) | (_, Value::Bool(true), false) => {
                        Value::Bool(true)
                    }
                    (Value::Null, _, _) | (_, Value::Null, _) => Value::Null,
                    (Value::Bool(a), Value::Bool(b), true) => Value::Bool(a && b),
                    (Value::Bool(a), Value::Bool(b), false) => Value::Bool(a || b),
                    _ => return Err(invalid("boolean operands required")),
                }
            }
            Not(v) => match v.evaluate(row)? {
                Value::Bool(v) => Value::Bool(!v),
                Value::Null => Value::Null,
                _ => return Err(invalid("boolean operand required")),
            },
            IsNull(v) => Value::Bool(matches!(v.evaluate(row)?, Value::Null)),
        })
    }
}
fn ordered(dtype: &DataType) -> bool {
    if let DataType::Map { key, value, .. } = dtype {
        return ordered(key) && ordered(value);
    }
    matches!(
        dtype,
        DataType::Null
            | DataType::Int64
            | DataType::Float64
            | DataType::Utf8
            | DataType::Bool
            | DataType::Timestamp
            | DataType::Date
    )
}
pub(super) fn numeric(op: &ArithmeticOpKind, a: Value, b: Value) -> Result<Value, Error> {
    use ArithmeticOpKind::*;
    Ok(match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Float64(a), Value::Float64(b)) => {
            Value::Float64(crate::arithmetic::evaluate_float64_arithmetic(op, a, b))
        }
        (Value::Int64(a), Value::Int64(b)) => Value::Int64(
            match op {
                Add => a.checked_add(b),
                Sub => a.checked_sub(b),
                Mul => a.checked_mul(b),
                Div => a.checked_div(b),
                Mod => a.checked_rem(b),
                Pow => u32::try_from(b).ok().and_then(|b| a.checked_pow(b)),
                Atan2 => None,
            }
            .ok_or_else(|| invalid("invalid integer arithmetic or overflow"))?,
        ),
        _ => return Err(invalid("arithmetic type mismatch")),
    })
}
#[derive(Clone, Debug)]
pub struct SortKey {
    pub column: usize,
    pub descending: bool,
    pub nulls_first: bool,
}
#[derive(Clone, Debug)]
pub enum Reduction {
    Count,
    Sum(usize),
    Avg(usize),
    Min(usize),
    Max(usize),
}
#[derive(Clone)]
enum Kind {
    Source(Vec<Batch>),
    Union,
    VectorToScalar {
        column: usize,
    },
    Project(Vec<Expression>),
    Filter(Expression),
    Limit {
        n: u64,
        offset: u64,
        groups: Vec<usize>,
    },
    Sort {
        keys: Vec<SortKey>,
        groups: Vec<usize>,
    },
    Window {
        intent: Box<planner_types::pre_asap::AggIntent<ColumnRef>>,
        coordinate: usize,
        value: usize,
        groups: Vec<usize>,
        window: Option<(i64, i64)>,
    },
    Aggregate {
        groups: Vec<usize>,
        measures: Vec<Reduction>,
    },
    SemiJoin {
        keys: Vec<(usize, usize)>,
    },
    Join {
        kind: planner_types::pre_asap::JoinKind,
        predicate: Box<super::expressions::CompiledExpression>,
    },
    SummaryBuild {
        family: SummaryFamilyType,
        value: usize,
        time: Option<usize>,
        groups: Vec<usize>,
    },
    KeyedSummaryBuild {
        family: SummaryFamilyType,
        value: usize,
        items: Vec<usize>,
        groups: Vec<usize>,
    },
    KeyedReadout {
        state: usize,
        k: usize,
    },
    SummaryMerge {
        state: usize,
        groups: Vec<usize>,
    },
    Readout {
        state: usize,
        statistic: crate::Statistic,
        parameters: std::collections::HashMap<String, String>,
    },
}
/// A bound operation has a fully checked input/output contract before execution.
#[derive(Clone)]
pub struct Operator {
    kind: Kind,
    inputs: Vec<Schema>,
    output: Schema,
}
impl Operator {
    pub fn source(output: Schema, batches: Vec<Batch>) -> Result<Self, Error> {
        super::values::validate_schema(&output)?;
        if batches.iter().any(|b| b.schema() != &output) {
            return Err(invalid("source schema mismatch"));
        }
        Ok(Self {
            kind: Kind::Source(batches),
            inputs: vec![],
            output,
        })
    }
    /// Union polls every input fairly, including branches sharing a producer.
    pub fn union(input: Schema, arity: usize) -> Result<Self, Error> {
        if arity == 0 {
            return Err(invalid("union needs at least one input"));
        }
        Ok(Self {
            kind: Kind::Union,
            inputs: vec![input.clone(); arity],
            output: input,
        })
    }
    pub fn scalar(value: Value, dtype: DataType) -> Result<Self, Error> {
        let schema = schema(vec![result_field(
            "value",
            dtype,
            matches!(value, Value::Null),
        )]);
        Self::source(
            schema.clone(),
            vec![Batch::try_new(schema, vec![vec![value]])?],
        )
    }
    /// PromQL scalar conversion: zero or multiple elements produce NaN.
    pub fn vector_to_scalar(input: Schema, column: usize) -> Result<Self, Error> {
        if plain(&input, column)? != (&DataType::Float64, false) {
            return Err(invalid("scalar conversion requires non-null Float64"));
        }
        Ok(Self {
            kind: Kind::VectorToScalar { column },
            inputs: vec![input],
            output: schema(vec![result_field("value", DataType::Float64, false)]),
        })
    }
    pub fn project(input: Schema, columns: Vec<(String, Expression)>) -> Result<Self, Error> {
        let fields = columns
            .iter()
            .map(|(name, e)| {
                let (t, n) = e.dtype(&input)?;
                Ok(result_field(name, t, n))
            })
            .collect::<Result<_, Error>>()?;
        Ok(Self {
            kind: Kind::Project(columns.into_iter().map(|(_, e)| e).collect()),
            inputs: vec![input],
            output: schema(fields),
        })
    }
    pub fn filter(input: Schema, predicate: Expression) -> Result<Self, Error> {
        if predicate.dtype(&input)?.0 != DataType::Bool {
            return Err(invalid("filter predicate must be boolean"));
        }
        Ok(Self {
            kind: Kind::Filter(predicate),
            inputs: vec![input.clone()],
            output: input,
        })
    }
    pub fn limit(input: Schema, n: u64, offset: u64, groups: Vec<usize>) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        Ok(Self {
            kind: Kind::Limit { n, offset, groups },
            inputs: vec![input.clone()],
            output: input,
        })
    }
    pub fn sort(input: Schema, keys: Vec<SortKey>, groups: Vec<usize>) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        for key in &keys {
            if !ordered(plain(&input, key.column)?.0) {
                return Err(invalid("unsupported sort type"));
            }
        }
        Ok(Self {
            kind: Kind::Sort { keys, groups },
            inputs: vec![input.clone()],
            output: input,
        })
    }
    pub fn aggregate(
        input: Schema,
        groups: Vec<usize>,
        measures: Vec<(String, Reduction)>,
    ) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        let mut fields = groups
            .iter()
            .map(|&i| input.fields[i].clone())
            .collect::<Vec<_>>();
        for (name, reduction) in &measures {
            let (t, n) = match reduction {
                Reduction::Count => (DataType::Int64, false),
                Reduction::Sum(i) | Reduction::Avg(i) => {
                    let (t, _) = plain(&input, *i)?;
                    if !matches!(t, DataType::Int64 | DataType::Float64) {
                        return Err(invalid("numeric aggregate input required"));
                    }
                    (
                        if matches!(reduction, Reduction::Avg(_)) {
                            DataType::Float64
                        } else {
                            t.clone()
                        },
                        false,
                    )
                }
                Reduction::Min(i) | Reduction::Max(i) => {
                    let (t, nullable) = plain(&input, *i)?;
                    if !ordered(t) {
                        return Err(invalid("ordered aggregate input required"));
                    }
                    (t.clone(), nullable || groups.is_empty())
                }
            };
            fields.push(result_field(name, t, n));
        }
        Ok(Self {
            kind: Kind::Aggregate {
                groups,
                measures: measures.into_iter().map(|(_, r)| r).collect(),
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    /// Bind a Planner temporal or histogram intent to explicit columns and window.
    pub fn window(
        input: Schema,
        intent: planner_types::pre_asap::AggIntent<ColumnRef>,
        coordinate: usize,
        value: usize,
        groups: Vec<usize>,
        window: Option<(i64, i64)>,
    ) -> Result<Self, Error> {
        use planner_types::pre_asap::AggIntent;
        validate_groups(&input, &groups)?;
        let histogram = matches!(intent, AggIntent::HistogramQuantile { .. });
        if !matches!(
            intent,
            AggIntent::Rate
                | AggIntent::Increase
                | AggIntent::Count { .. }
                | AggIntent::Sum { col: None }
                | AggIntent::Avg { col: None }
                | AggIntent::Min { col: None }
                | AggIntent::Max { col: None }
                | AggIntent::HistogramQuantile { .. }
        ) {
            return Err(invalid(
                "unsupported temporal intent or unresolved value column",
            ));
        }
        let coordinate_type = if histogram {
            DataType::Float64
        } else {
            DataType::Timestamp
        };
        if plain(&input, coordinate)? != (&coordinate_type, false)
            || plain(&input, value)? != (&DataType::Float64, false)
        {
            return Err(invalid("window coordinate/value schema mismatch"));
        }
        if (!histogram && !matches!(window, Some((start, end)) if start < end))
            || (histogram && window.is_some())
        {
            return Err(invalid("invalid temporal window"));
        }
        let mut fields = groups
            .iter()
            .map(|i| input.fields[*i].clone())
            .collect::<Vec<_>>();
        fields.push(result_field(
            "value",
            if matches!(intent, AggIntent::Count { .. }) {
                DataType::Int64
            } else {
                DataType::Float64
            },
            false,
        ));
        Ok(Self {
            kind: Kind::Window {
                intent: Box::new(intent),
                coordinate,
                value,
                groups,
                window,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }

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
            super::expressions::CompiledExpression::compile(&predicate.0, &schema(joined.clone()))?;
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
    pub fn summary_build(
        input: Schema,
        family: SummaryFamilyType,
        value: usize,
        time: Option<usize>,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        super::values::validate_family(&family)?;
        validate_groups(&input, &groups)?;
        if plain(&input, value)? != (&DataType::Float64, false) {
            return Err(invalid("summary numeric update requires non-null Float64"));
        }
        if let Some(time) = time {
            if plain(&input, time)? != (&DataType::Timestamp, false) {
                return Err(invalid("summary time column must be a timestamp"));
            }
        }
        if time.is_none()
            && matches!(
                family,
                SummaryFamilyType::ExactAggregate(
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
        fields.push(SummaryField {
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
    pub fn keyed_summary_build(
        input: Schema,
        family: SummaryFamilyType,
        value: usize,
        items: Vec<usize>,
        groups: Vec<usize>,
    ) -> Result<Self, Error> {
        use planner_types::post_asap::{SketchAlgorithm, SketchParams};
        super::values::validate_family(&family)?;
        let SummaryFamilyType::Sketch(kind, _) = &family else {
            return Err(invalid("keyed sketch required"));
        };
        if kind.algorithm() != &SketchAlgorithm::CmsWithHeap
            || !matches!(kind.params(), SketchParams::CmsWithHeap { .. })
        {
            return Err(invalid(
                "Float64 weighted keyed construction currently supports CMS with heap",
            ));
        }
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
        fields.push(SummaryField {
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
        use planner_types::post_asap::{SketchAlgorithm, SketchParams};
        let SummaryFamilyType::Sketch(kind, _) = &field(&input, state)?.dtype else {
            return Err(invalid("keyed readout requires summary state"));
        };
        let SketchParams::CmsWithHeap { heap_size, .. } = kind.params() else {
            return Err(invalid("unsupported keyed readout family"));
        };
        if kind.algorithm() != &SketchAlgorithm::CmsWithHeap
            || k > *heap_size as usize
            || output.fields.len() <= input.fields.len()
        {
            return Err(invalid("invalid keyed readout shape or capacity"));
        }
        if state + 1 != input.fields.len()
            || output.fields[..state] != input.fields[..state]
            || output.fields.last().unwrap().dtype != SummaryFamilyType::Plain(DataType::Float64)
        {
            return Err(invalid(
                "keyed readout must preserve partitions and return a Float64 score",
            ));
        }
        super::values::validate_schema(&output)?;
        Ok(Self {
            kind: Kind::KeyedReadout { state, k },
            inputs: vec![input],
            output,
        })
    }
    pub fn summary_merge(input: Schema, state: usize, groups: Vec<usize>) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        super::values::validate_family(&field(&input, state)?.dtype)?;
        if matches!(field(&input, state)?.dtype, SummaryFamilyType::Plain(_)) {
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
    pub fn readout(
        input: Schema,
        state: usize,
        statistic: crate::Statistic,
        parameters: std::collections::HashMap<String, String>,
    ) -> Result<Self, Error> {
        super::values::validate_family(&field(&input, state)?.dtype)?;
        if matches!(field(&input, state)?.dtype, SummaryFamilyType::Plain(_)) {
            return Err(invalid("summary state required"));
        }
        validate_readout(&field(&input, state)?.dtype, statistic, &parameters)?;
        let mut fields = input.fields.clone();
        let result_type = if matches!(
            fields[state].dtype,
            SummaryFamilyType::ExactAggregate(planner_types::post_asap::ExactKind::Count, _)
        ) {
            DataType::Int64
        } else {
            DataType::Float64
        };
        fields[state] = result_field("value", result_type, false);
        Ok(Self {
            kind: Kind::Readout {
                state,
                statistic,
                parameters,
            },
            inputs: vec![input],
            output: schema(fields),
        })
    }
    pub(crate) fn with_output_schema(mut self, output: Schema) -> Result<Self, Error> {
        if self.output.fields.len() != output.fields.len()
            || self
                .output
                .fields
                .iter()
                .zip(&output.fields)
                .any(|(actual, declared)| {
                    actual.dtype != declared.dtype || (actual.nullable && !declared.nullable)
                })
        {
            return Err(invalid("native output type differs from Planner output"));
        }
        if output.time_index.is_some_and(|i| {
            i >= output.fields.len()
                || output.fields[i].dtype != SummaryFamilyType::Plain(DataType::Timestamp)
        }) {
            return Err(invalid("invalid output time column"));
        }
        self.output = output;
        Ok(self)
    }
    pub fn schema(&self) -> Schema {
        self.output.clone()
    }
}
fn validate_groups(input: &Schema, groups: &[usize]) -> Result<(), Error> {
    for &i in groups {
        plain(input, i)?;
    }
    if groups
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != groups.len()
    {
        return Err(invalid("duplicate group columns"));
    }
    Ok(())
}
async fn collect_rows(
    mut input: Input<'_, Batch>,
    context: &RunContext,
) -> Result<(Vec<Vec<Value>>, Vec<Reservation>), Error> {
    let mut rows = Vec::new();
    let mut reservations = Vec::new();
    while let Some(batch) = input.next().await {
        let batch = batch?;
        reservations.push(context.reserve(batch.bytes())?);
        rows.extend(batch.rows().iter().cloned());
    }
    Ok((rows, reservations))
}
impl PhysicalOperator<Batch, Schema> for Operator {
    fn name(&self) -> &str {
        match self.kind {
            Kind::Source(_) => "Source",
            Kind::Union => "Union",
            Kind::VectorToScalar { .. } => "VectorToScalar",
            Kind::Project(_) => "Project",
            Kind::Filter(_) => "Filter",
            Kind::Limit { .. } => "Limit",
            Kind::Sort { .. } => "Sort",
            Kind::Aggregate { .. } => "Aggregate",
            Kind::Window { .. } => "WindowAggregate",
            Kind::SemiJoin { .. } => "SemiJoin",
            Kind::Join { .. } => "RelationalJoin",
            Kind::SummaryBuild { .. } | Kind::KeyedSummaryBuild { .. } => "SummaryAgg",
            Kind::KeyedReadout { .. } => "SummaryEstimate",
            Kind::SummaryMerge { .. } => "SummaryMerge",
            Kind::Readout { .. } => "SummaryReadout",
        }
    }
    fn input_schemas(&self) -> Vec<Schema> {
        self.inputs.clone()
    }
    fn output_schema(&self) -> Schema {
        self.output.clone()
    }
    fn output_bytes(&self, value: &Batch) -> usize {
        value.bytes()
    }
    fn start<'a>(
        &'a self,
        mut inputs: Vec<Input<'a, Batch>>,
        context: RunContext,
    ) -> Result<OutputStream<'a, Batch>, Error> {
        let output = self.output.clone();
        if let Kind::Source(batches) = &self.kind {
            return Ok(futures::stream::iter(batches.iter().cloned().map(Ok)).boxed_local());
        }
        if matches!(self.kind, Kind::Union) {
            return Ok(futures::stream::select_all(inputs)
                .map(|batch| batch.map(|batch| batch.value().clone()))
                .boxed_local());
        }
        if let Kind::Join { kind, predicate } = &self.kind {
            let right = inputs.pop().ok_or_else(|| invalid("right input missing"))?;
            let left = inputs.pop().ok_or_else(|| invalid("left input missing"))?;
            return Ok(futures::stream::once(async move {
                use planner_types::pre_asap::JoinKind;
                let ((left, _left_memory), (right, _right_memory)) = futures::try_join!(
                    collect_rows(left, &context),
                    collect_rows(right, &context)
                )?;
                let mut result = Vec::new();
                let mut right_matched = vec![false; right.len()];
                for left_row in &left {
                    let mut matched = false;
                    for (i, right_row) in right.iter().enumerate() {
                        let mut joined = left_row.clone();
                        joined.extend(right_row.iter().cloned());
                        if *kind == JoinKind::Cross
                            || matches!(predicate.evaluate(&joined)?, Value::Bool(true))
                        {
                            matched = true;
                            right_matched[i] = true;
                            match kind {
                                JoinKind::Semi => {
                                    result.push(left_row.clone());
                                    break;
                                }
                                JoinKind::Anti => break,
                                _ => result.push(joined),
                            }
                        }
                    }
                    if !matched {
                        match kind {
                            JoinKind::Left | JoinKind::Full => {
                                let mut joined = left_row.clone();
                                joined.resize(
                                    joined.len() + self.inputs[1].fields.len(),
                                    Value::Null,
                                );
                                result.push(joined);
                            }
                            JoinKind::Anti => result.push(left_row.clone()),
                            _ => {}
                        }
                    }
                }
                if matches!(kind, JoinKind::Right | JoinKind::Full) {
                    for (matched, row) in right_matched.into_iter().zip(right) {
                        if !matched {
                            let mut joined = vec![Value::Null; self.inputs[0].fields.len()];
                            joined.extend(row);
                            result.push(joined);
                        }
                    }
                }
                Batch::try_new(output, result)
            })
            .boxed_local());
        }
        if let Kind::SemiJoin { keys } = &self.kind {
            let right = inputs.pop().ok_or_else(|| invalid("right input missing"))?;
            let left = inputs.pop().ok_or_else(|| invalid("left input missing"))?;
            return Ok(futures::stream::once(async move {
                // Poll both branches together: either may depend on a common producer.
                let ((left, _left_memory), (right, _right_memory)) = futures::try_join!(
                    collect_rows(left, &context),
                    collect_rows(right, &context)
                )?;
                let right_cols = keys.iter().map(|(_, r)| *r).collect::<Vec<_>>();
                let left_cols = keys.iter().map(|(l, _)| *l).collect::<Vec<_>>();
                let members = right
                    .iter()
                    .filter(|row| right_cols.iter().all(|&i| !matches!(row[i], Value::Null)))
                    .map(|r| group_key(r, &right_cols))
                    .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
                let rows = left
                    .into_iter()
                    .filter_map(|r| match group_key(&r, &left_cols) {
                        Ok(k)
                            if left_cols.iter().all(|&i| !matches!(r[i], Value::Null))
                                && members.contains(&k) =>
                        {
                            Some(Ok(r))
                        }
                        Ok(_) => None,
                        Err(e) => Some(Err(e)),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Batch::try_new(output, rows)
            })
            .boxed_local());
        }
        let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
        match &self.kind {
            Kind::VectorToScalar { column } => Ok(futures::stream::once(async move {
                let mut input = input;
                let mut value = f64::NAN;
                let mut count = 0usize;
                while let Some(batch) = input.next().await {
                    for row in batch?.rows() {
                        count = count.saturating_add(1);
                        if let Value::Float64(v) = row[*column] {
                            value = v;
                        }
                    }
                }
                Batch::try_new(
                    output,
                    vec![vec![Value::Float64(if count == 1 {
                        value
                    } else {
                        f64::NAN
                    })]],
                )
            })
            .boxed_local()),
            Kind::Project(expressions) => Ok(input
                .map(move |batch| {
                    let batch = batch?;
                    let rows = batch
                        .rows()
                        .iter()
                        .map(|r| {
                            expressions
                                .iter()
                                .map(|e| e.evaluate(r))
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Batch::try_new(output.clone(), rows)
                })
                .boxed_local()),
            Kind::Filter(predicate) => Ok(input
                .map(move |batch| {
                    let batch = batch?;
                    let mut rows = Vec::new();
                    for row in batch.rows() {
                        if matches!(predicate.evaluate(row)?, Value::Bool(true)) {
                            rows.push(row.clone());
                        }
                    }
                    Batch::try_new(output.clone(), rows)
                })
                .boxed_local()),
            Kind::Limit { n, offset, groups } => {
                let counts = BTreeMap::<Vec<Vec<u8>>, u64>::new();
                Ok(futures::stream::try_unfold(
                    (input, counts, Vec::<Reservation>::new(), false),
                    move |(mut input, mut counts, mut memory, done)| {
                        let output = output.clone();
                        let context = context.clone();
                        async move {
                            if done || *n == 0 {
                                return Ok(None);
                            }
                            let Some(batch) = input.next().await else {
                                return Ok(None);
                            };
                            let batch = batch?;
                            let mut rows = Vec::new();
                            for row in batch.rows() {
                                let key = group_key(row, groups)?;
                                if !counts.contains_key(&key) {
                                    memory.push(
                                        context.reserve(
                                            key.iter()
                                                .map(|part| {
                                                    part.len() + std::mem::size_of::<Vec<u8>>()
                                                })
                                                .sum::<usize>()
                                                + 64,
                                        )?,
                                    );
                                }
                                let count = counts.entry(key).or_default();
                                if *count >= *offset && count.saturating_sub(*offset) < *n {
                                    rows.push(row.clone());
                                }
                                *count = count.saturating_add(1);
                            }
                            let done = groups.is_empty()
                                && counts
                                    .get(&vec![])
                                    .is_some_and(|count| count.saturating_sub(*offset) >= *n);
                            Ok(Some((
                                Batch::try_new(output, rows)?,
                                (input, counts, memory, done),
                            )))
                        }
                    },
                )
                .boxed_local())
            }
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
                            .downcast_ref::<crate::summary_operators::weighted_cms::WeightedCms>()
                            .ok_or_else(|| invalid("weighted CMS typed state required"))?;
                        for items in summary.rows(*k) {
                            let mut values = row[..*state].to_vec();
                            values.extend(items);
                            rows.push(values);
                        }
                    }
                    Batch::try_new(output.clone(), rows)
                })
                .boxed_local()),
            Kind::Readout {
                state,
                statistic,
                parameters,
            } => Ok(input
                .map(move |batch| {
                    let batch = batch?;
                    let mut rows = batch.rows().to_vec();
                    for row in &mut rows {
                        let Value::Summary { state: summary, .. } = &row[*state] else {
                            return Err(invalid("summary value required"));
                        };
                        row[*state] = if output.fields[*state].dtype
                            == SummaryFamilyType::Plain(DataType::Int64)
                        {
                            let count = summary.aux_stats().count.ok_or_else(|| {
                                Error::Operator("exact count state lacks an integer count".into())
                            })?;
                            Value::Int64(
                                i64::try_from(count).map_err(|_| {
                                    Error::Operator("exact count exceeds Int64".into())
                                })?,
                            )
                        } else {
                            Value::Float64(
                                summary
                                    .query_statistic(*statistic, &None, parameters)
                                    .map_err(|e| Error::Operator(e.to_string()))?,
                            )
                        };
                    }
                    Batch::try_new(output.clone(), rows)
                })
                .boxed_local()),
            _ => Ok(futures::stream::once(async move {
                let (rows, _memory) = collect_rows(input, &context).await?;
                let result = match &self.kind {
                    Kind::Sort { keys, groups } => {
                        let mut grouped = BTreeMap::<Vec<Vec<u8>>, Vec<Vec<Value>>>::new();
                        for row in rows {
                            for key in keys {
                                if matches!(row[key.column], Value::Map(_))
                                    && nested_nan(&row[key.column])
                                {
                                    return Err(invalid("NaN in collection sort key"));
                                }
                            }
                            grouped
                                .entry(group_key(&row, groups)?)
                                .or_default()
                                .push(row);
                        }
                        let mut result = Vec::new();
                        for mut rows in grouped.into_values() {
                            rows.sort_by(|a, b| compare_rows(a, b, keys));
                            result.extend(rows);
                        }
                        result
                    }
                    Kind::Window {
                        intent,
                        coordinate,
                        value,
                        groups,
                        window,
                    } => {
                        super::temporal::reduce(rows, intent, groups, *coordinate, *value, *window)?
                    }
                    Kind::Aggregate { groups, measures } => {
                        reduce(rows, groups, measures, &self.inputs[0])?
                    }
                    Kind::SummaryMerge { state, groups } => merge_summary(rows, *state, groups)?,
                    _ => return Err(invalid("unexpected blocking operation")),
                };
                Batch::try_new(output, result)
            })
            .boxed_local()),
        }
    }
}
fn compare_rows(a: &[Value], b: &[Value], keys: &[SortKey]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    for key in keys {
        let (a, b) = (&a[key.column], &b[key.column]);
        let order = match (a, b) {
            (Value::Null, Value::Null) => Equal,
            (Value::Null, _) => {
                if key.nulls_first {
                    Less
                } else {
                    Greater
                }
            }
            (_, Value::Null) => {
                if key.nulls_first {
                    Greater
                } else {
                    Less
                }
            }
            (Value::Float64(a), Value::Float64(b)) if a.is_nan() || b.is_nan() => {
                match (a.is_nan(), b.is_nan()) {
                    (true, true) => Equal,
                    (true, false) => Greater,
                    _ => Less,
                }
            }
            _ => {
                let order = a.compare(b).expect("bound ordered types");
                if key.descending {
                    order.reverse()
                } else {
                    order
                }
            }
        };
        if order != Equal {
            return order;
        }
    }
    Equal
}
fn reduce(
    rows: Vec<Vec<Value>>,
    groups: &[usize],
    measures: &[Reduction],
    input: &Schema,
) -> Result<Vec<Vec<Value>>, Error> {
    let mut grouped = BTreeMap::<Vec<Vec<u8>>, Vec<Vec<Value>>>::new();
    if rows.is_empty() && groups.is_empty() {
        grouped.insert(vec![], vec![]);
    }
    for row in rows {
        grouped
            .entry(group_key(&row, groups)?)
            .or_default()
            .push(row);
    }
    grouped
        .into_values()
        .map(|rows| {
            let mut result = groups
                .iter()
                .map(|&i| rows[0][i].clone())
                .collect::<Vec<_>>();
            for measure in measures {
                result.push(reduce_one(&rows, measure, input)?);
            }
            Ok(result)
        })
        .collect()
}
fn reduce_one(rows: &[Vec<Value>], measure: &Reduction, input: &Schema) -> Result<Value, Error> {
    let column = match measure {
        Reduction::Count => {
            return Ok(Value::Int64(
                i64::try_from(rows.len()).map_err(|_| invalid("count overflow"))?,
            ))
        }
        Reduction::Sum(i) | Reduction::Avg(i) | Reduction::Min(i) | Reduction::Max(i) => *i,
    };
    let values = rows
        .iter()
        .map(|r| &r[column])
        .filter(|v| !matches!(v, Value::Null))
        .collect::<Vec<_>>();
    if matches!(measure, Reduction::Min(_) | Reduction::Max(_)) {
        if plain(input, column)?.0 == &DataType::Float64 {
            // Match exact-state kernels: ignore NaN when a numeric value exists.
            let mut best: Option<f64> = None;
            for value in values {
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
    let count = values.len();
    let dtype = plain(input, column)?.0;
    if dtype == &DataType::Int64 {
        let sum = values.into_iter().try_fold(0i128, |sum, v| {
            let Value::Int64(v) = v else {
                return Err(invalid("integer aggregate value required"));
            };
            sum.checked_add(i128::from(*v))
                .ok_or_else(|| invalid("integer aggregate overflow"))
        })?;
        return if matches!(measure, Reduction::Avg(_)) {
            Ok(Value::Float64(sum as f64 / count as f64))
        } else {
            Ok(Value::Int64(
                i64::try_from(sum).map_err(|_| invalid("integer sum overflow"))?,
            ))
        };
    }
    let sum = values
        .into_iter()
        .map(|v| {
            if let Value::Float64(v) = v {
                *v
            } else {
                unreachable!()
            }
        })
        .sum::<f64>();
    Ok(Value::Float64(if matches!(measure, Reduction::Avg(_)) {
        sum / count as f64
    } else {
        sum
    }))
}
async fn build_keyed_summary(
    mut input: Input<'_, Batch>,
    family: &SummaryFamilyType,
    value: usize,
    items: &[usize],
    groups: &[usize],
    context: &RunContext,
) -> Result<Vec<Vec<Value>>, Error> {
    use crate::{summary_operators::weighted_cms::WeightedCms, AggregateCore};
    use planner_types::post_asap::SketchParams;
    let SummaryFamilyType::Sketch(kind, _) = family else {
        unreachable!()
    };
    let SketchParams::CmsWithHeap {
        width,
        depth,
        heap_size,
    } = kind.params()
    else {
        unreachable!()
    };
    let mut states = BTreeMap::<Vec<Vec<u8>>, (Vec<Value>, WeightedCms, Reservation, usize)>::new();
    while let Some(batch) = input.next().await {
        let batch = batch?;
        for row in batch.rows() {
            if context.is_cancelled() {
                return Err(Error::Operator("execution cancelled".into()));
            }
            let key = group_key(row, groups)?;
            if !states.contains_key(&key) {
                let labels = groups.iter().map(|&i| row[i].clone()).collect::<Vec<_>>();
                let overhead = labels.iter().map(Value::bytes).sum::<usize>()
                    + key.iter().map(|v| v.len() + 24).sum::<usize>()
                    + 128;
                let bytes = (*width as usize)
                    .checked_mul(*depth as usize)
                    .and_then(|n| n.checked_mul(8))
                    .and_then(|n| n.checked_add(overhead))
                    .ok_or_else(|| invalid("weighted CMS memory size overflow"))?;
                let reservation = context.reserve(bytes)?;
                states.insert(
                    key.clone(),
                    (
                        labels,
                        WeightedCms::new(*width as usize, *depth as usize, *heap_size as usize)?,
                        reservation,
                        overhead,
                    ),
                );
            }
            let (_, summary, reservation, overhead) = states.get_mut(&key).unwrap();
            let Value::Float64(weight) = row[value] else {
                return Err(invalid("weighted CMS weight type"));
            };
            summary.update(
                &items.iter().map(|&i| row[i].clone()).collect::<Vec<_>>(),
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

async fn build_summary(
    mut input: Input<'_, Batch>,
    family: &SummaryFamilyType,
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
    let mut states = BTreeMap::<Vec<Vec<u8>>, State>::new();
    if groups.is_empty() {
        states.insert(vec![], create(vec![], 0)?);
    }
    let ordered_time = matches!(
        family,
        SummaryFamilyType::ExactAggregate(
            planner_types::post_asap::ExactKind::Rate
                | planner_types::post_asap::ExactKind::Increase,
            _
        )
    );
    while let Some(batch) = input.next().await {
        let batch = batch?;
        for row in batch.rows() {
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
            let Value::Float64(value) = row[value] else {
                return Err(invalid("summary update type"));
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

fn merge_summary(
    rows: Vec<Vec<Value>>,
    state_column: usize,
    groups: &[usize],
) -> Result<Vec<Vec<Value>>, Error> {
    type GroupState = (Vec<Value>, SummaryFamilyType, Arc<dyn crate::AggregateCore>);
    let mut states: BTreeMap<Vec<Vec<u8>>, GroupState> = BTreeMap::new();
    for row in rows {
        let Value::Summary { family, state } = &row[state_column] else {
            return Err(invalid("summary state required"));
        };
        let key = group_key(&row, groups)?;
        if let Some((_, expected, existing)) = states.get_mut(&key) {
            if expected != family {
                return Err(invalid("incompatible summary family"));
            }
            *existing = Arc::from(
                existing
                    .merge_with(state.as_ref())
                    .map_err(|e| Error::Operator(e.to_string()))?,
            );
        } else {
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

fn validate_readout(
    family: &SummaryFamilyType,
    statistic: crate::Statistic,
    parameters: &std::collections::HashMap<String, String>,
) -> Result<(), Error> {
    use crate::Statistic as S;
    use planner_types::post_asap::{ExactKind as E, SketchAlgorithm as A};
    let supported = match family {
        SummaryFamilyType::ExactAggregate(kind, _) => matches!(
            (kind, statistic),
            (E::Sum, S::Sum)
                | (E::Count, S::Count)
                | (E::Min, S::Min)
                | (E::Max, S::Max)
                | (E::Rate, S::Rate)
                | (E::Increase, S::Increase)
        ),
        SummaryFamilyType::Sketch(kind, _) => match kind.algorithm() {
            A::Kll => statistic == S::Quantile,
            A::DDSketch => matches!(statistic, S::Quantile | S::Count),
            A::Hll => matches!(statistic, S::Cardinality | S::Count),
            _ => false,
        },
        _ => false,
    };
    if !supported {
        return Err(invalid(
            "readout is not implemented for this summary family",
        ));
    }
    if statistic == S::Quantile
        && !parameters
            .get("quantile")
            .and_then(|s| s.parse::<f64>().ok())
            .is_some_and(|q| (0.0..=1.0).contains(&q))
    {
        return Err(invalid("quantile readout requires quantile in [0,1]"));
    }
    Ok(())
}

fn nested_nan(value: &Value) -> bool {
    match value {
        Value::Float64(value) => value.is_nan(),
        Value::Map(values) => values
            .iter()
            .any(|(key, value)| nested_nan(key) || nested_nan(value)),
        Value::List(values) | Value::Struct(values) => values.iter().any(nested_nan),
        _ => false,
    }
}
