//! Planning-only DataFusion UDFs for ClickHouse's collection functions:
//! `map(...)`, `mapConcat`, `arrayElement` and `tupleElement` over Map, List
//! and Struct values. They let DataFusion's SQL planner resolve these calls,
//! with return types from the canonical signature rules; lowering turns them
//! into IR scalar functions, and DataFusion never executes them (evaluation
//! is the query engine's responsibility).
use super::types::{arrow_to_dtype, dtype_to_arrow, scalar_value_to_asap};
use asap_types::ir::scalar::{element_access_type, struct_field_type};
use asap_types::ir::ScalarExpr;
use asap_types::pre_asap::scalar_type_rules::MapScalarFunction;
use asap_types::pre_asap::{Field, Schema};
use datafusion::arrow::datatypes::{DataType, Field as ArrowField, FieldRef};
use datafusion::common::{DataFusionError, Result, ScalarValue as DfScalarValue};
use datafusion::logical_expr::{
    ColumnarValue, ReturnFieldArgs, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature,
    TypeSignature, Volatility,
};
use datafusion::prelude::SessionContext;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// The name ClickHouse's `map(...)` is planned under. DataFusion's SQL planner
/// reserves `map` for its own constructor (and rejects `map()`), so
/// `clickhouse_ast::normalize` renames the call and lowering restores `map`.
pub(super) const MAP_UDF_NAME: &str = "asap_map_construct";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollectionFunction {
    Map(MapScalarFunction),
    Element,
    StructField,
}

pub(super) fn register(context: &SessionContext) {
    for (name, function) in [
        (
            MAP_UDF_NAME,
            CollectionFunction::Map(MapScalarFunction::Construct),
        ),
        (
            "mapconcat",
            CollectionFunction::Map(MapScalarFunction::Concat),
        ),
        ("arrayelement", CollectionFunction::Element),
        ("tupleelement", CollectionFunction::StructField),
    ] {
        context.register_udf(ScalarUDF::from(CollectionUdf {
            name,
            function,
            signature: match function {
                CollectionFunction::Map(MapScalarFunction::Construct) => Signature::one_of(
                    vec![TypeSignature::Exact(vec![]), TypeSignature::VariadicAny],
                    Volatility::Immutable,
                ),
                CollectionFunction::Map(MapScalarFunction::Access)
                | CollectionFunction::Element
                | CollectionFunction::StructField => Signature::any(2, Volatility::Immutable),
                CollectionFunction::Map(MapScalarFunction::Concat) => {
                    Signature::variadic_any(Volatility::Immutable)
                }
            },
        }));
    }
}
#[derive(Debug, PartialEq, Eq)]
struct CollectionUdf {
    name: &'static str,
    function: CollectionFunction,
    signature: Signature,
}
// `function` is determined by `name` at registration, so hashing the name and
// signature agrees with the derived `Eq`.
impl Hash for CollectionUdf {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.signature.hash(state);
    }
}
impl CollectionUdf {
    fn output(
        &self,
        args: &[DataType],
        nullable: &[bool],
        literals: Option<&[Option<&DfScalarValue>]>,
    ) -> Result<(DataType, bool)> {
        let inputs = args
            .iter()
            .zip(nullable)
            .map(|(dtype, null)| {
                arrow_to_dtype(dtype)
                    .map(|dtype| (dtype, *null))
                    .map_err(|e| DataFusionError::Plan(e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let (dtype, nullable) = if matches!(
            self.function,
            CollectionFunction::Element | CollectionFunction::StructField
        ) {
            // DataFusion asks for argument-dependent types before canonical
            // expression binding. Reuse the shared resolver over typed argument
            // slots; final canonical binding also validates literal selectors.
            let schema = Schema::new(
                inputs
                    .into_iter()
                    .enumerate()
                    .map(|(index, (dtype, nullable))| {
                        Field::plain(format!("argument_{index}"), dtype, nullable)
                    })
                    .collect(),
            );
            let args = (0..schema.fields.len())
                .map(|index| {
                    if let Some(Some(value)) = literals.and_then(|args| args.get(index)) {
                        scalar_value_to_asap(value)
                            .map(ScalarExpr::Literal)
                            .map_err(|error| DataFusionError::Plan(error.to_string()))
                    } else {
                        Ok(ScalarExpr::Column(index))
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            match self.function {
                CollectionFunction::Element => element_access_type(&args, &schema),
                CollectionFunction::StructField => struct_field_type(&args, &schema),
                CollectionFunction::Map(_) => unreachable!(),
            }
        } else if let CollectionFunction::Map(function) = self.function {
            function.output_type(&inputs)
        } else {
            unreachable!()
        }
        .map_err(DataFusionError::Plan)?;
        Ok((dtype_to_arrow(&dtype), nullable))
    }
}
impl ScalarUDFImpl for CollectionUdf {
    fn name(&self) -> &str {
        self.name
    }
    fn signature(&self) -> &Signature {
        &self.signature
    }
    fn return_type(&self, args: &[DataType]) -> Result<DataType> {
        self.output(
            args,
            &args
                .iter()
                .map(|dtype| *dtype == DataType::Null)
                .collect::<Vec<_>>(),
            None,
        )
        .map(|output| output.0)
    }
    fn return_field_from_args(&self, args: ReturnFieldArgs) -> Result<FieldRef> {
        let types = args
            .arg_fields
            .iter()
            .map(|field| field.data_type().clone())
            .collect::<Vec<_>>();
        let nullable = args
            .arg_fields
            .iter()
            .map(|field| field.is_nullable())
            .collect::<Vec<_>>();
        let (dtype, nullable) = self.output(&types, &nullable, Some(args.scalar_arguments))?;
        Ok(Arc::new(ArrowField::new(self.name, dtype, nullable)))
    }
    fn invoke_with_args(&self, _args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        Err(DataFusionError::NotImplemented(
            "collection UDF cannot execute; use a capable query engine or external exact sub-DAG"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collection_udf_explicitly_refuses_physical_execution() {
        let udf = CollectionUdf {
            name: MAP_UDF_NAME,
            function: CollectionFunction::Map(MapScalarFunction::Construct),
            signature: Signature::any(0, Volatility::Immutable),
        };
        let args = ScalarFunctionArgs {
            args: vec![],
            arg_fields: vec![],
            number_rows: 1,
            return_field: Arc::new(ArrowField::new("map", DataType::Null, true)),
            config_options: Default::default(),
        };
        assert!(matches!(
            udf.invoke_with_args(args),
            Err(DataFusionError::NotImplemented(_))
        ));
        let result = udf.return_type(&[]).unwrap();
        let (expected, _) = MapScalarFunction::Construct.output_type(&[]).unwrap();
        assert_eq!(result, dtype_to_arrow(&expected));
    }
}
