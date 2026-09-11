//! Type bridges between DataFusion's Arrow types and the canonical `DataType`, plus
//! the SQL table catalog used to register tables with DataFusion and to carry
//! resolved leaf schemas into the canonical, unresolved tree.

use std::collections::HashMap;

use datafusion::arrow::datatypes::{
    DataType as ArrowDataType, Field, Fields, Schema as ArrowSchema,
};
use datafusion::common::ScalarValue as DfScalarValue;

use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::ScalarValue;

use crate::error::SqlError as LoweringError;

/// Table catalog for SQL lowering: table name → resolved canonical [`Schema`].
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

    /// Builder: register `name` with its resolved canonical schema.
    pub fn with_table(mut self, name: impl Into<String>, schema: Schema) -> Self {
        self.tables.insert(name.into(), schema);
        self
    }
}

pub(super) fn scalar_value_to_asap(sv: &DfScalarValue) -> Result<ScalarValue, LoweringError> {
    match sv {
        DfScalarValue::Int64(Some(v)) => Ok(ScalarValue::Int64(*v)),
        DfScalarValue::Int32(Some(v)) => Ok(ScalarValue::Int64(*v as i64)),
        DfScalarValue::Int16(Some(v)) => Ok(ScalarValue::Int64(*v as i64)),
        DfScalarValue::Int8(Some(v)) => Ok(ScalarValue::Int64(*v as i64)),
        DfScalarValue::UInt64(Some(v)) => i64::try_from(*v).map(ScalarValue::Int64).map_err(|_| {
            LoweringError::InvalidExpression(format!("UInt64 value {v} overflows i64"))
        }),
        DfScalarValue::UInt32(Some(v)) => Ok(ScalarValue::Int64(*v as i64)),
        DfScalarValue::Float64(Some(v)) => Ok(ScalarValue::Float64(*v)),
        DfScalarValue::Float32(Some(v)) => Ok(ScalarValue::Float64(*v as f64)),
        DfScalarValue::Utf8(Some(s)) | DfScalarValue::LargeUtf8(Some(s)) => {
            Ok(ScalarValue::Utf8(s.clone()))
        }
        DfScalarValue::Boolean(Some(b)) => Ok(ScalarValue::Boolean(*b)),
        _ if sv.is_null() => Ok(ScalarValue::Null),
        _ => Err(LoweringError::InvalidExpression(format!(
            "unsupported scalar: {sv:?}"
        ))),
    }
}

/// Arrow → the canonical `DataType` (used for `CAST` targets). Deliberately narrow.
pub(super) fn arrow_to_dtype(dt: &ArrowDataType) -> Result<DataType, LoweringError> {
    match dt {
        ArrowDataType::Int64
        | ArrowDataType::Int32
        | ArrowDataType::Int16
        | ArrowDataType::Int8 => Ok(DataType::Int64),
        ArrowDataType::Float64 | ArrowDataType::Float32 => Ok(DataType::Float64),
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => Ok(DataType::Utf8),
        ArrowDataType::Boolean => Ok(DataType::Bool),
        ArrowDataType::Timestamp(_, _) => Ok(DataType::Timestamp),
        ArrowDataType::List(element) => Ok(DataType::List {
            element: Box::new(Column::new(
                element.name(),
                arrow_to_dtype(element.data_type())?,
                element.is_nullable(),
            )),
        }),
        ArrowDataType::Struct(fields) => Ok(DataType::Struct {
            fields: fields
                .iter()
                .map(|field| {
                    Ok(Column::new(
                        field.name(),
                        arrow_to_dtype(field.data_type())?,
                        field.is_nullable(),
                    ))
                })
                .collect::<Result<Vec<_>, LoweringError>>()?,
        }),
        ArrowDataType::Map(entries, _) => {
            let ArrowDataType::Struct(fields) = entries.data_type() else {
                return Err(LoweringError::UnsupportedFeature(
                    "map entries must be a struct".into(),
                ));
            };
            if fields.len() != 2 || fields[0].is_nullable() {
                return Err(LoweringError::UnsupportedFeature(
                    "map entries require a non-null key and a value".into(),
                ));
            }
            Ok(DataType::Map {
                key: Box::new(arrow_to_dtype(fields[0].data_type())?),
                value: Box::new(arrow_to_dtype(fields[1].data_type())?),
                value_nullable: fields[1].is_nullable(),
            })
        }
        other => Err(LoweringError::UnsupportedFeature(format!(
            "Arrow type: {other:?}"
        ))),
    }
}

/// The canonical `DataType` → Arrow (for registering catalog tables with DataFusion).
pub(super) fn dtype_to_arrow(dt: &DataType) -> ArrowDataType {
    match dt {
        DataType::Int64 => ArrowDataType::Int64,
        DataType::Float64 => ArrowDataType::Float64,
        DataType::Utf8 => ArrowDataType::Utf8,
        DataType::Bool => ArrowDataType::Boolean,
        DataType::List { element } => ArrowDataType::List(std::sync::Arc::new(Field::new(
            &element.name,
            dtype_to_arrow(&element.dtype),
            element.nullable,
        ))),
        DataType::Struct { fields } => ArrowDataType::Struct(
            fields
                .iter()
                .map(|field| Field::new(&field.name, dtype_to_arrow(&field.dtype), field.nullable))
                .collect::<Vec<_>>()
                .into(),
        ),
        DataType::Map {
            key,
            value,
            value_nullable,
        } => ArrowDataType::Map(
            std::sync::Arc::new(Field::new(
                "entries",
                ArrowDataType::Struct(
                    vec![
                        Field::new("key", dtype_to_arrow(key), false),
                        Field::new("value", dtype_to_arrow(value), *value_nullable),
                    ]
                    .into(),
                ),
                false,
            )),
            false,
        ),
        DataType::Timestamp => {
            ArrowDataType::Timestamp(datafusion::arrow::datatypes::TimeUnit::Millisecond, None)
        }
    }
}

/// Build an Arrow schema from a canonical [`Schema`] (column name + type + nullability).
pub(super) fn schema_to_arrow(schema: &Schema) -> ArrowSchema {
    let fields: Fields = schema
        .columns
        .iter()
        .map(|c: &Column| Field::new(&c.name, dtype_to_arrow(&c.dtype), c.nullable))
        .collect();
    ArrowSchema::new(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nested map values and value nullability survive catalog registration.
    #[test]
    fn nested_map_schema_round_trip() {
        let map = DataType::Map {
            key: Box::new(DataType::Utf8),
            value: Box::new(DataType::Map {
                key: Box::new(DataType::Int64),
                value: Box::new(DataType::Float64),
                value_nullable: true,
            }),
            value_nullable: false,
        };
        assert_eq!(arrow_to_dtype(&dtype_to_arrow(&map)).unwrap(), map);
        let encoded = serde_json::to_string(&map).unwrap();
        assert_eq!(serde_json::from_str::<DataType>(&encoded).unwrap(), map);
    }
}

#[cfg(test)]
mod collection_tests {
    use super::*;
    #[test]
    fn nested_collections_preserve_field_names_order_and_nullability() {
        let dtype = DataType::Struct {
            fields: vec![
                Column::new(
                    "samples",
                    DataType::List {
                        element: Box::new(Column::new(
                            "sample",
                            DataType::Struct {
                                fields: vec![
                                    Column::new("timestamp", DataType::Timestamp, false),
                                    Column::new("value", DataType::Float64, true),
                                    Column::new(
                                        "labels",
                                        DataType::Map {
                                            key: Box::new(DataType::Utf8),
                                            value: Box::new(DataType::List {
                                                element: Box::new(Column::new(
                                                    "label_value",
                                                    DataType::Utf8,
                                                    false,
                                                )),
                                            }),
                                            value_nullable: true,
                                        },
                                        true,
                                    ),
                                ],
                            },
                            true,
                        )),
                    },
                    false,
                ),
                Column::new("optional", DataType::Int64, true),
            ],
        };
        let arrow = dtype_to_arrow(&dtype);
        assert_eq!(arrow_to_dtype(&arrow).unwrap(), dtype);
        assert_eq!(dtype_to_arrow(&arrow_to_dtype(&arrow).unwrap()), arrow);
        let encoded = serde_json::to_string(&dtype).unwrap();
        assert_eq!(serde_json::from_str::<DataType>(&encoded).unwrap(), dtype);
    }
    #[test]
    fn empty_struct_and_nonnullable_list_element_roundtrip() {
        let dtype = DataType::List {
            element: Box::new(Column::new(
                "empty",
                DataType::Struct { fields: vec![] },
                false,
            )),
        };
        assert_eq!(arrow_to_dtype(&dtype_to_arrow(&dtype)).unwrap(), dtype);
    }
}
