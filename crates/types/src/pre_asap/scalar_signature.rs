//! Shared type rules for structural map scalar expressions.
//! Execution must separately implement the documented ordering/default semantics.
use super::schema::DataType;

/// Names are resolved once against this closed builtin set; unknown functions
/// remain outside these type rules. Map access keeps the first duplicate key
/// and returns the value type's default when the key is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapScalarFunction {
    Construct,
    Concat,
    Access,
}
impl MapScalarFunction {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "map" => Some(Self::Construct),
            "mapconcat" => Some(Self::Concat),
            "asap_map_access" => Some(Self::Access),
            _ => None,
        }
    }
    pub fn output_type(self, args: &[(DataType, bool)]) -> Result<(DataType, bool), String> {
        match self {
            Self::Construct => {
                let (pairs, remainder) = args.as_chunks::<2>();
                if !remainder.is_empty() {
                    return Err("map construction requires key/value pairs".into());
                }
                let mut key = DataType::Null;
                let mut value = DataType::Null;
                let mut value_nullable = false;
                for pair in pairs {
                    if pair[0].1 || pair[0].0 == DataType::Null {
                        return Err("map keys must be non-null".into());
                    }
                    key = common_type(&key, &pair[0].0)?;
                    value = common_type(&value, &pair[1].0)?;
                    value_nullable |= pair[1].1 || pair[1].0 == DataType::Null;
                }
                Ok((
                    DataType::Map {
                        key: Box::new(key),
                        value: Box::new(value),
                        value_nullable,
                    },
                    false,
                ))
            }
            Self::Concat => {
                if args.is_empty() {
                    return Err("map concatenation requires at least one map".into());
                }
                let mut key = DataType::Null;
                let mut value = DataType::Null;
                let mut value_nullable = false;
                for (argument, nullable) in args {
                    if *nullable {
                        return Err("nullable map containers are unsupported".into());
                    }
                    let DataType::Map {
                        key: k,
                        value: v,
                        value_nullable: n,
                    } = argument
                    else {
                        return Err("map concatenation requires map arguments".into());
                    };
                    key = common_type(&key, k)?;
                    value = common_type(&value, v)?;
                    value_nullable |= *n;
                }
                Ok((
                    DataType::Map {
                        key: Box::new(key),
                        value: Box::new(value),
                        value_nullable,
                    },
                    false,
                ))
            }
            Self::Access => {
                let [(map, map_nullable), (index, index_nullable)] = args else {
                    return Err("map access requires a map and key".into());
                };
                if *map_nullable {
                    return Err("nullable map containers are unsupported".into());
                }
                let DataType::Map {
                    key,
                    value,
                    value_nullable,
                } = map
                else {
                    return Err("map access requires a map".into());
                };
                if **key == DataType::Null {
                    return Err("map lookup requires a concrete map key type".into());
                }
                if *index != DataType::Null && common_type(key, index)? != **key {
                    return Err("map lookup key requires a lossy or unsupported coercion".into());
                }
                Ok((
                    (**value).clone(),
                    *value_nullable
                        || *index_nullable
                        || *index == DataType::Null
                        || **value == DataType::Null,
                ))
            }
        }
    }
}
fn common_type(left: &DataType, right: &DataType) -> Result<DataType, String> {
    if left == right || *right == DataType::Null {
        return Ok(left.clone());
    }
    if *left == DataType::Null {
        return Ok(right.clone());
    }
    Err(format!(
        "incompatible map scalar types: {left:?} and {right:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_map_is_bottom_typed_and_concat_resolves_it() {
        let empty = MapScalarFunction::Construct.output_type(&[]).unwrap();
        assert_eq!(
            empty,
            (
                DataType::Map {
                    key: Box::new(DataType::Null),
                    value: Box::new(DataType::Null),
                    value_nullable: false
                },
                false
            )
        );
        assert!(MapScalarFunction::Access
            .output_type(&[empty.clone(), (DataType::Utf8, false)])
            .is_err());
        let concrete = MapScalarFunction::Construct
            .output_type(&[(DataType::Utf8, false), (DataType::Int64, false)])
            .unwrap();
        assert_eq!(
            MapScalarFunction::Concat
                .output_type(&[empty, concrete.clone()])
                .unwrap(),
            concrete.clone()
        );
        assert_eq!(
            MapScalarFunction::Access
                .output_type(&[concrete, (DataType::Utf8, false)])
                .unwrap(),
            (DataType::Int64, false)
        );
    }
    #[test]
    fn nullable_lookup_and_invalid_signatures_are_explicit() {
        let map = MapScalarFunction::Construct
            .output_type(&[(DataType::Utf8, false), (DataType::Int64, true)])
            .unwrap();
        assert_eq!(
            MapScalarFunction::Access
                .output_type(&[map, (DataType::Utf8, false)])
                .unwrap(),
            (DataType::Int64, true)
        );
        let nonnull = MapScalarFunction::Construct
            .output_type(&[(DataType::Utf8, false), (DataType::Int64, false)])
            .unwrap();
        assert_eq!(
            MapScalarFunction::Access
                .output_type(&[nonnull, (DataType::Utf8, true)])
                .unwrap(),
            (DataType::Int64, true)
        );
        assert!(MapScalarFunction::Construct
            .output_type(&[(DataType::Utf8, true), (DataType::Int64, false)])
            .is_err());
        assert!(MapScalarFunction::Concat
            .output_type(&[(DataType::Int64, false)])
            .is_err());
        // ClickHouse can choose Variant(Float64, Int64), not lossless Float64.
        assert!(MapScalarFunction::Construct
            .output_type(&[
                (DataType::Utf8, false),
                (DataType::Int64, false),
                (DataType::Utf8, false),
                (DataType::Float64, false),
            ])
            .is_err());
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use crate::pre_asap::{Column, ProjectItem, QueryExpr, ScalarValue, Schema, Source};
    use std::rc::Rc;
    fn project(expr: QueryExpr) -> QueryExpr {
        QueryExpr::Project {
            cols: vec![ProjectItem {
                alias: Some("result".into()),
                expr,
            }],
            qualifier: None,
            child: Rc::new(QueryExpr::Scan {
                source: Source::Table {
                    table_ref: "t".into(),
                },
                predicates: vec![],
                schema: Schema::new(vec![
                    Column::new("k", DataType::Utf8, false),
                    Column::new("v", DataType::Int64, true),
                ]),
            }),
        }
    }
    #[test]
    fn canonical_projection_uses_map_signature_and_rejects_invalid_arity() {
        let map = QueryExpr::FunctionCall {
            name: "map".into(),
            args: vec![QueryExpr::Column(0), QueryExpr::Column(1)],
        };
        let schema = project(map.clone()).output_schema().unwrap();
        assert_eq!(
            schema.columns[0].dtype,
            DataType::Map {
                key: Box::new(DataType::Utf8),
                value: Box::new(DataType::Int64),
                value_nullable: true
            }
        );
        assert!(!schema.columns[0].nullable);
        let lookup = QueryExpr::FunctionCall {
            name: "asap_map_access".into(),
            args: vec![map, QueryExpr::Literal(ScalarValue::Utf8("missing".into()))],
        };
        assert_eq!(
            project(lookup).output_schema().unwrap().columns[0],
            Column::new("result", DataType::Int64, true)
        );
        assert!(project(QueryExpr::FunctionCall {
            name: "map".into(),
            args: vec![QueryExpr::Column(0)]
        })
        .output_schema()
        .is_err());
    }
}

/// Resolve the bounded canonical `asap_struct_field(struct, selector)` operation.
/// Selectors are positive 1-based literal ordinals or exact literal field names.
/// The existing Struct fields remain the sole authority for type/nullability.
/// Dynamic/negative/defaulted selectors and nullable containers are intentionally
/// unsupported here; this is not a claim of complete native tupleElement support.
pub fn struct_field_type(
    args: &[super::QueryExpr],
    schema: &super::Schema,
) -> Result<(DataType, bool), String> {
    use super::{QueryExpr, ScalarValue};
    let [input, selector] = args else {
        return Err("struct field access requires a struct and constant selector".into());
    };
    let (dtype, nullable) = input
        .scalar_type(schema)
        .map_err(|error| error.to_string())?;
    if nullable {
        return Err("nullable struct container access is unsupported".into());
    }
    let DataType::Struct { fields } = dtype else {
        return Err("struct field access requires a Struct input".into());
    };
    let field = match selector {
        QueryExpr::Literal(ScalarValue::Int64(index)) if *index > 0 => usize::try_from(*index - 1)
            .ok()
            .and_then(|index| fields.get(index))
            .ok_or("struct field ordinal is out of bounds")?,
        QueryExpr::Literal(ScalarValue::Utf8(name)) => {
            let mut matches = fields.iter().filter(|field| field.name == *name);
            let field = matches.next().ok_or("struct field name does not exist")?;
            if matches.next().is_some() {
                return Err("struct field name is ambiguous".into());
            }
            field
        }
        _ => {
            return Err(
                "struct field selector must be a positive ordinal or field-name literal".into(),
            )
        }
    };
    Ok((field.dtype.clone(), field.nullable))
}

#[cfg(test)]
mod struct_field_tests {
    use super::*;
    use crate::pre_asap::{Column, QueryExpr, ScalarValue, Schema};
    fn schema() -> Schema {
        Schema::new(vec![Column::new(
            "record",
            DataType::Struct {
                fields: vec![
                    Column::new("ts", DataType::Int64, false),
                    Column::new(
                        "values",
                        DataType::List {
                            element: Box::new(Column::new("item", DataType::Float64, true)),
                        },
                        true,
                    ),
                ],
            },
            false,
        )])
    }
    fn access(selector: QueryExpr) -> QueryExpr {
        QueryExpr::FunctionCall {
            name: "asap_struct_field".into(),
            args: vec![QueryExpr::Column(0), selector],
        }
    }
    #[test]
    fn field_access_reuses_nested_field_type_and_nullability() {
        let schema = schema();
        assert_eq!(
            access(QueryExpr::Literal(ScalarValue::Int64(1)))
                .scalar_type(&schema)
                .unwrap(),
            (DataType::Int64, false)
        );
        let named = access(QueryExpr::Literal(ScalarValue::Utf8("values".into())));
        let ordinal = access(QueryExpr::Literal(ScalarValue::Int64(2)));
        assert_eq!(
            named.scalar_type(&schema).unwrap(),
            ordinal.scalar_type(&schema).unwrap()
        );
        assert_eq!(
            named.scalar_type(&schema).unwrap(),
            (
                DataType::List {
                    element: Box::new(Column::new("item", DataType::Float64, true))
                },
                true
            )
        );
        let roundtrip: QueryExpr =
            serde_json::from_str(&serde_json::to_string(&named).unwrap()).unwrap();
        assert_eq!(roundtrip, named);
    }
    #[test]
    fn unsupported_field_access_is_an_error_not_placeholder_typing() {
        for selector in [
            QueryExpr::Column(0),
            QueryExpr::Literal(ScalarValue::Int64(0)),
            QueryExpr::Literal(ScalarValue::Int64(-1)),
            QueryExpr::Literal(ScalarValue::Int64(3)),
            QueryExpr::Literal(ScalarValue::Utf8("missing".into())),
        ] {
            assert!(access(selector).scalar_type(&schema()).is_err());
        }
        let mut nullable = schema();
        nullable.columns[0].nullable = true;
        assert!(access(QueryExpr::Literal(ScalarValue::Int64(1)))
            .scalar_type(&nullable)
            .is_err());
    }
}
