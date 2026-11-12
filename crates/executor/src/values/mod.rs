//! Runtime values preserve Planner schemas; summary states are typed values too.
use crate::AggregateCore;
use crate::Error;
use planner_types::ir::schema::{
    DataType, Field as SummaryField, FieldDataType as SummaryFamilyType, Schema,
};
use std::{cmp::Ordering, sync::Arc};
pub(crate) mod arrow_columns;
mod batch;
mod state_column;
pub use batch::{Batch, BatchColumn};
pub use state_column::StateColumn;
pub(crate) use state_column::StateStorage;
/// Shared ownership of schema metadata; the field model is identical at planning
/// and execution time.
pub type SchemaRef = Arc<Schema>;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(#[serde(with = "planner_types::serde_f64")] f64),
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
    #[serde(skip)]
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

pub(crate) use crate::capability::validate_native_family as validate_family;

/// Whether `state` is a state of `family`: the kernel's state type of the
/// family's algorithm. θ is not part of a family, so a state's shape is its
/// own; a merge refuses states of different shapes.
pub(super) fn validate_state(
    family: &SummaryFamilyType,
    state: &dyn AggregateCore,
) -> Result<(), Error> {
    use crate::summary_kernels::weighted_frequency::{FrequencyAlgorithm, WeightedFrequency};
    use crate::summary_kernels::{
        count_min_sketch::CountMinSketchAccumulator, datasketches_kll::DatasketchesKLLAccumulator,
        dd_sketch::DDSketchAccumulator, exact::ExactAccumulator, hll_sketch::HllSketchAccumulator,
        kmv::KmvAccumulator, univmon::UnivMonAccumulator, HydraCmsGroup,
    };
    use planner_types::summary_type::SketchAlgorithm as A;
    validate_family(family)?;
    let state = state.as_any();
    let frequency = |algorithm| {
        state
            .downcast_ref::<WeightedFrequency>()
            .is_some_and(|s| s.algorithm() == algorithm && s.shape().2 > 0)
    };
    let valid = match family {
        SummaryFamilyType::ExactAggregate(..) => state
            .downcast_ref::<ExactAccumulator>()
            .is_some_and(|s| s.family() == family && !s.is_keyed()),
        SummaryFamilyType::Sketch(algorithm) => match algorithm {
            A::HydraCms => state.is::<HydraCmsGroup>(),
            A::CmsWithHeap => frequency(FrequencyAlgorithm::Cms),
            A::CountSketchWithHeap => frequency(FrequencyAlgorithm::CountSketch),
            A::Kll => state.is::<DatasketchesKLLAccumulator>(),
            A::DDSketch => state.is::<DDSketchAccumulator>(),
            A::Hll => state.is::<HllSketchAccumulator>(),
            A::Kmv => state.is::<KmvAccumulator>(),
            A::UnivMon => state.is::<UnivMonAccumulator>(),
            A::Cms => state.is::<CountMinSketchAccumulator>(),
            _ => false,
        },
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Error::Invalid(
            "state payload differs from declared family".into(),
        ))
    }
}

pub(crate) fn validate_schema(schema: &SchemaRef) -> Result<(), Error> {
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

pub(crate) fn field(schema: &SchemaRef, column: usize) -> Result<&SummaryField, Error> {
    schema
        .fields
        .get(column)
        .ok_or_else(|| Error::Invalid("column out of range".into()))
}
pub(crate) fn plain(schema: &SchemaRef, column: usize) -> Result<(&DataType, bool), Error> {
    let f = field(schema, column)?;
    let SummaryFamilyType::Plain(dtype) = &f.dtype else {
        return Err(Error::Invalid("plain value required".into()));
    };
    Ok((dtype, f.nullable))
}

#[cfg(test)]
mod weighted_state_tests;
