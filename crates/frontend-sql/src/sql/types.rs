//! Type bridges between DataFusion's Arrow types and the L3 `DataType`, plus
//! the SQL table catalog used to register tables with DataFusion and to carry
//! resolved leaf schemas into the relational L2 tree.

use std::collections::HashMap;

use datafusion::arrow::datatypes::{
    DataType as ArrowDataType, Field, Fields, Schema as ArrowSchema,
};
use datafusion::common::ScalarValue;

use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::L3Scalar;

use crate::error::SqlError as LoweringError;

/// Table catalog for SQL lowering: table name → resolved L3 [`Schema`].
///
/// Used twice: to register Arrow-backed `MemTable`s so DataFusion can resolve
/// `SELECT … FROM t`, and to attach each table's schema directly onto the
/// canonical `Scan` (`schema: Some(_)`) so the Binder doesn't need to
/// usage-derive it.
#[derive(Debug, Clone, Default)]
pub struct SqlCatalog {
    pub tables: HashMap<String, Schema>,
}

impl SqlCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: register `name` with its resolved L3 schema.
    pub fn with_table(mut self, name: impl Into<String>, schema: Schema) -> Self {
        self.tables.insert(name.into(), schema);
        self
    }
}

pub(super) fn scalar_value_to_l3(sv: &ScalarValue) -> Result<L3Scalar, LoweringError> {
    match sv {
        ScalarValue::Int64(Some(v)) => Ok(L3Scalar::Int64(*v)),
        ScalarValue::Int32(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::Int16(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::Int8(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::UInt64(Some(v)) => i64::try_from(*v).map(L3Scalar::Int64).map_err(|_| {
            LoweringError::InvalidExpression(format!("UInt64 value {v} overflows i64"))
        }),
        ScalarValue::UInt32(Some(v)) => Ok(L3Scalar::Int64(*v as i64)),
        ScalarValue::Float64(Some(v)) => Ok(L3Scalar::Float64(*v)),
        ScalarValue::Float32(Some(v)) => Ok(L3Scalar::Float64(*v as f64)),
        ScalarValue::Utf8(Some(s)) | ScalarValue::LargeUtf8(Some(s)) => {
            Ok(L3Scalar::Utf8(s.clone()))
        }
        ScalarValue::Boolean(Some(b)) => Ok(L3Scalar::Boolean(*b)),
        _ if sv.is_null() => Ok(L3Scalar::Null),
        _ => Err(LoweringError::InvalidExpression(format!(
            "unsupported scalar: {sv:?}"
        ))),
    }
}

/// Arrow → L3 `DataType` (used for `CAST` targets). L3 is deliberately narrow.
pub(super) fn arrow_to_l3(dt: &ArrowDataType) -> Result<DataType, LoweringError> {
    match dt {
        ArrowDataType::Int64
        | ArrowDataType::Int32
        | ArrowDataType::Int16
        | ArrowDataType::Int8 => Ok(DataType::Int64),
        ArrowDataType::Float64 | ArrowDataType::Float32 => Ok(DataType::Float64),
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => Ok(DataType::Utf8),
        ArrowDataType::Boolean => Ok(DataType::Bool),
        ArrowDataType::Timestamp(_, _) => Ok(DataType::Timestamp),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "Arrow type: {other:?}"
        ))),
    }
}

/// L3 `DataType` → Arrow (for registering catalog tables with DataFusion).
pub(super) fn l3_to_arrow(dt: &DataType) -> ArrowDataType {
    match dt {
        DataType::Int64 => ArrowDataType::Int64,
        DataType::Float64 => ArrowDataType::Float64,
        DataType::Utf8 => ArrowDataType::Utf8,
        DataType::Bool => ArrowDataType::Boolean,
        DataType::Timestamp => {
            ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None)
        }
    }
}

/// Build an Arrow schema from an L3 [`Schema`] (column name + type + nullability).
pub(super) fn schema_to_arrow(schema: &Schema) -> ArrowSchema {
    let fields: Fields = schema
        .columns
        .iter()
        .map(|c: &Column| Field::new(&c.name, l3_to_arrow(&c.dtype), c.nullable))
        .collect();
    ArrowSchema::new(fields)
}
