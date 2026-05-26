use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType as ArrowDataType, Field, Fields, Schema, TimeUnit};
use datafusion::common::ScalarValue;

use asap_control_core::intent_algebra::schema::{L3DataType, TableSchema};
use asap_control_core::intent_algebra::L3Scalar;

use crate::error::LoweringError;

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
        // Typed nulls and untyped null both become L3Scalar::Null
        _ if sv.is_null() => Ok(L3Scalar::Null),
        _ => Err(LoweringError::InvalidExpression(format!(
            "unsupported scalar: {sv:?}"
        ))),
    }
}

pub(super) fn arrow_to_l3(dt: &ArrowDataType) -> Result<L3DataType, LoweringError> {
    match dt {
        ArrowDataType::Int64
        | ArrowDataType::Int32
        | ArrowDataType::Int16
        | ArrowDataType::Int8 => Ok(L3DataType::Int64),
        ArrowDataType::Float64 | ArrowDataType::Float32 => Ok(L3DataType::Float64),
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => Ok(L3DataType::Utf8),
        ArrowDataType::Boolean => Ok(L3DataType::Boolean),
        ArrowDataType::Timestamp(_, _) => Ok(L3DataType::Timestamp),
        ArrowDataType::Duration(_) => Ok(L3DataType::Duration),
        other => Err(LoweringError::UnsupportedFeature(format!(
            "Arrow type in cast: {other:?}"
        ))),
    }
}

pub(super) fn table_schema_to_arrow(schema: &TableSchema) -> Schema {
    let fields: Fields = schema
        .columns
        .iter()
        .map(|c| Field::new(&c.name, l3_to_arrow(&c.data_type), c.nullable))
        .collect();
    Schema::new(fields)
}

pub(super) fn l3_to_arrow(dt: &L3DataType) -> ArrowDataType {
    match dt {
        L3DataType::Int64 => ArrowDataType::Int64,
        L3DataType::Float64 => ArrowDataType::Float64,
        L3DataType::Utf8 => ArrowDataType::Utf8,
        L3DataType::Boolean => ArrowDataType::Boolean,
        L3DataType::Timestamp => ArrowDataType::Timestamp(TimeUnit::Millisecond, None),
        L3DataType::Duration => ArrowDataType::Duration(TimeUnit::Millisecond),
        L3DataType::Map(k, v) => ArrowDataType::Map(
            Arc::new(Field::new(
                "entries",
                ArrowDataType::Struct(Fields::from(vec![
                    Field::new("key", l3_to_arrow(k), false),
                    Field::new("value", l3_to_arrow(v), true),
                ])),
                false,
            )),
            false,
        ),
        L3DataType::List(item) => {
            ArrowDataType::List(Arc::new(Field::new("item", l3_to_arrow(item), true)))
        }
    }
}
