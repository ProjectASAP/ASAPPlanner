//! Runtime values preserve Planner schemas; summary states are typed values too.
use super::Error;
use crate::AggregateCore;
use planner_types::{
    post_asap::{SummaryFamilyType, SummarySchema},
    pre_asap::DataType,
};
use std::{cmp::Ordering, sync::Arc};
pub type Schema = Arc<SummarySchema>;
#[derive(Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    Utf8(Arc<str>),
    Timestamp(i64),
    Date(i32),
    Interval {
        months: i32,
        days: i32,
        nanos: i64,
    },
    List(Arc<[Value]>),
    Struct(Arc<[Value]>),
    Map(Arc<[(Value, Value)]>),
    Summary {
        family: SummaryFamilyType,
        state: Arc<dyn AggregateCore>,
    },
}
impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Summary { family, .. } => f.debug_tuple("Summary").field(family).finish(),
            _ => write!(f, "{:?}", self.key()),
        }
    }
}
impl Value {
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match self {
                Self::Utf8(s) => s.len(),
                Self::List(v) | Self::Struct(v) => v.iter().map(Self::bytes).sum(),
                Self::Map(v) => v.iter().map(|(k, v)| k.bytes() + v.bytes()).sum(),
                Self::Summary { state, .. } => state.approx_memory_bytes(),
                _ => 0,
            }
    }
    pub fn matches(&self, dtype: &DataType, nullable: bool) -> bool {
        if matches!(self, Self::Null) {
            return nullable || matches!(dtype, DataType::Null);
        }
        match (self, dtype) {
            (Self::Bool(_), DataType::Bool)
            | (Self::Int64(_), DataType::Int64)
            | (Self::Float64(_), DataType::Float64)
            | (Self::Utf8(_), DataType::Utf8)
            | (Self::Timestamp(_), DataType::Timestamp)
            | (Self::Date(_), DataType::Date)
            | (Self::Interval { .. }, DataType::Interval) => true,
            (Self::List(v), DataType::List { element }) => v
                .iter()
                .all(|v| v.matches(&element.dtype, element.nullable)),
            (Self::Struct(v), DataType::Struct { fields }) => {
                v.len() == fields.len()
                    && v.iter()
                        .zip(fields)
                        .all(|(v, f)| v.matches(&f.dtype, f.nullable))
            }
            (
                Self::Map(v),
                DataType::Map {
                    key,
                    value,
                    value_nullable,
                },
            ) => v
                .iter()
                .all(|(k, v)| k.matches(key, false) && v.matches(value, *value_nullable)),
            _ => false,
        }
    }
    /// Stable typed equality key. Zero signs and NaN payloads form one group.
    pub fn key(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        macro_rules! number {
            ($tag:expr,$v:expr) => {{
                out.push($tag);
                out.extend_from_slice(&$v.to_le_bytes());
            }};
        }
        match self {
            Self::Null => out.push(0),
            Self::Bool(v) => out.extend([1, *v as u8]),
            Self::Int64(v) => number!(2, v),
            Self::Float64(v) => {
                let bits = if *v == 0. {
                    0
                } else if v.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    v.to_bits()
                };
                number!(3, bits);
            }
            Self::Utf8(v) => {
                out.push(4);
                out.extend(v.as_bytes());
            }
            Self::Timestamp(v) => number!(5, v),
            Self::Date(v) => number!(6, v),
            Self::Interval {
                months,
                days,
                nanos,
            } => {
                number!(7, months);
                number!(8, days);
                number!(9, nanos);
            }
            Self::List(v) | Self::Struct(v) => {
                out.push(if matches!(self, Self::List(_)) {
                    10
                } else {
                    11
                });
                for v in v.iter() {
                    let key = v.key()?;
                    out.extend((key.len() as u64).to_le_bytes());
                    out.extend(key);
                }
            }
            Self::Map(v) => {
                out.push(12);
                for (k, v) in v.iter() {
                    for value in [k, v] {
                        let key = value.key()?;
                        out.extend((key.len() as u64).to_le_bytes());
                        out.extend(key);
                    }
                }
            }
            Self::Summary { .. } => {
                return Err(Error::Invalid(
                    "summary states cannot be grouping keys".into(),
                ))
            }
        }
        Ok(out)
    }
    pub fn compare(&self, other: &Self) -> Result<Ordering, Error> {
        Ok(match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Int64(a), Self::Int64(b)) | (Self::Timestamp(a), Self::Timestamp(b)) => a.cmp(b),
            (Self::Float64(a), Self::Float64(b)) => {
                if a == b {
                    Ordering::Equal
                } else {
                    a.total_cmp(b)
                }
            }
            (Self::Utf8(a), Self::Utf8(b)) => a.cmp(b),
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            (Self::Date(a), Self::Date(b)) => a.cmp(b),
            (Self::Map(left), Self::Map(right)) => {
                let mut result = Ordering::Equal;
                for ((lk, lv), (rk, rv)) in left.iter().zip(right.iter()) {
                    result = lk.compare(rk)?;
                    if result != Ordering::Equal {
                        break;
                    }
                    result = match (lv, rv) {
                        (Self::Null, Self::Null) => Ordering::Equal,
                        (Self::Null, _) => Ordering::Greater,
                        (_, Self::Null) => Ordering::Less,
                        _ => lv.compare(rv)?,
                    };
                    if result != Ordering::Equal {
                        break;
                    }
                }
                if result == Ordering::Equal {
                    left.len().cmp(&right.len())
                } else {
                    result
                }
            }
            _ => {
                return Err(Error::Operator(
                    "values do not have a supported common ordering".into(),
                ))
            }
        })
    }
}
#[derive(Clone, Debug)]
pub struct Batch {
    schema: Schema,
    rows: Vec<Vec<Value>>,
}
impl Batch {
    pub fn try_new(schema: Schema, rows: Vec<Vec<Value>>) -> Result<Self, Error> {
        validate_schema(&schema)?;
        for row in &rows {
            if row.len() != schema.fields.len() {
                return Err(Error::Invalid(
                    "row width differs from Planner schema".into(),
                ));
            }
            for (value, field) in row.iter().zip(&schema.fields) {
                let matches = match (&field.dtype, value) {
                    (SummaryFamilyType::Plain(dtype), value) => {
                        value.matches(dtype, field.nullable)
                    }
                    (expected, Value::Summary { family, state }) => {
                        expected == family && validate_state(family, state.as_ref()).is_ok()
                    }
                    _ => false,
                };
                if !matches {
                    return Err(Error::Invalid(format!(
                        "value differs from type of {}",
                        field.name
                    )));
                }
            }
        }
        Ok(Self { schema, rows })
    }
    pub fn schema(&self) -> &Schema {
        &self.schema
    }
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }
    pub fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .rows
                .iter()
                .flat_map(|r| r.iter())
                .map(Value::bytes)
                .sum::<usize>()
    }
}
pub(crate) fn group_key(row: &[Value], columns: &[usize]) -> Result<Vec<Vec<u8>>, Error> {
    columns
        .iter()
        .map(|&i| {
            row.get(i)
                .ok_or_else(|| Error::Invalid("group column out of range".into()))?
                .key()
        })
        .collect()
}

pub(crate) fn validate_family(family: &SummaryFamilyType) -> Result<(), Error> {
    use planner_types::post_asap::SketchAlgorithm as A;
    if let SummaryFamilyType::Sketch(kind, grouping) = family {
        if let planner_types::post_asap::SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        } = kind.params()
        {
            return if kind.algorithm() == &A::CmsWithHeap
                && *width > 0
                && *depth > 0
                && *heap_size > 0
                && grouping == &Default::default()
            {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "invalid weighted CMS family or grouping strategy".into(),
                ))
            };
        }
    }
    match family {
        SummaryFamilyType::ExactAggregate(..) => {}
        SummaryFamilyType::Sketch(kind, _)
            if matches!(kind.algorithm(), A::Kll | A::DDSketch | A::Hll) => {}
        _ => {
            return Err(Error::Invalid(
                "summary family has no native DAG state implementation".into(),
            ))
        }
    }
    crate::capability::validate_summary_kernel(
        family,
        &planner_types::post_asap::SummaryUpdate::column(
            planner_types::pre_asap::ColumnRef::SampleValue,
        ),
        &Default::default(),
    )
    .map_err(Error::Invalid)
}
fn validate_state(family: &SummaryFamilyType, state: &dyn AggregateCore) -> Result<(), Error> {
    use crate::summary_operators::{
        datasketches_kll_accumulator::DatasketchesKLLAccumulator,
        dd_sketch_accumulator::DDSketchAccumulator, exact_accumulator::ExactAccumulator,
        hll_sketch_accumulator::HllSketchAccumulator,
    };
    use planner_types::post_asap::SketchParams;
    validate_family(family)?;
    let valid = match family {
        SummaryFamilyType::Sketch(kind, _)
            if matches!(kind.params(), SketchParams::CmsWithHeap { .. }) =>
        {
            let SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            } = kind.params()
            else {
                unreachable!()
            };
            state
                .as_any()
                .downcast_ref::<crate::summary_operators::weighted_cms::WeightedCms>()
                .is_some_and(|state| {
                    state.shape() == (*width as usize, *depth as usize, *heap_size as usize)
                })
        }

        SummaryFamilyType::ExactAggregate(..) => {
            state
                .as_any()
                .downcast_ref::<ExactAccumulator>()
                .is_some_and(|s| s.family() == family && !s.is_keyed())
                || (matches!(
                    family,
                    SummaryFamilyType::ExactAggregate(
                        planner_types::post_asap::ExactKind::Sum,
                        planner_types::post_asap::ExactParams::Sum
                    )
                ) && state
                    .as_any()
                    .is::<crate::summary_operators::SumAccumulator>())
        }
        SummaryFamilyType::Sketch(kind, _) => match kind.params() {
            SketchParams::Kll { k } => state
                .as_any()
                .downcast_ref::<DatasketchesKLLAccumulator>()
                .is_some_and(|s| u32::from(s.inner.k()) == *k),
            SketchParams::DDSketch { alpha } => state
                .as_any()
                .downcast_ref::<DDSketchAccumulator>()
                .is_some_and(|s| s.inner.alpha == *alpha && s.sample_p == 1.0),
            SketchParams::Hll { precision } => state
                .as_any()
                .downcast_ref::<HllSketchAccumulator>()
                .is_some_and(|s| s.inner.precision == u32::from(*precision) && s.sample_p == 1.0),
            _ => false,
        },
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Error::Invalid(
            "state payload differs from declared family, parameters or population layout".into(),
        ))
    }
}

pub(crate) fn validate_schema(schema: &Schema) -> Result<(), Error> {
    if schema.time_index.is_some_and(|index| {
        schema
            .fields
            .get(index)
            .is_none_or(|field| field.dtype != SummaryFamilyType::Plain(DataType::Timestamp))
    }) {
        return Err(Error::Invalid(
            "time index must name a Timestamp column".into(),
        ));
    }
    for field in &schema.fields {
        if !matches!(field.dtype, SummaryFamilyType::Plain(_)) {
            validate_family(&field.dtype)?;
            if field.nullable {
                return Err(Error::Invalid(
                    "nullable summary states are not supported".into(),
                ));
            }
        }
    }
    Ok(())
}
