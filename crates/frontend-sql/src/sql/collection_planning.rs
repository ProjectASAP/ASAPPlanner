//! DataFusion planning adapters. Types come from the canonical signature rules;
//! physical evaluation deliberately remains the query engine's responsibility.
use super::types::{arrow_to_dtype, dtype_to_arrow, scalar_value_to_asap};
use asap_types::ir::scalar::scalar_type_rules::MapScalarFunction;
use asap_types::ir::scalar::{element_access_type, struct_field_type};
use asap_types::ir::schema::{Field, Schema};
use asap_types::ir::ScalarExpr;
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
pub(super) const MAP_PLANNING_NAME: &str = "asap_map_construct";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanningFunction {
    Map(MapScalarFunction),
    Element,
    StructField,
}

pub(super) fn register(context: &SessionContext) {
    for (name, function) in [
        (
            MAP_PLANNING_NAME,
            PlanningFunction::Map(MapScalarFunction::Construct),
        ),
        (
            "mapconcat",
            PlanningFunction::Map(MapScalarFunction::Concat),
        ),
        ("arrayelement", PlanningFunction::Element),
        ("tupleelement", PlanningFunction::StructField),
    ] {
        context.register_udf(ScalarUDF::from(CollectionPlanningFunction {
            name,
            function,
            signature: match function {
                PlanningFunction::Map(MapScalarFunction::Construct) => Signature::one_of(
                    vec![TypeSignature::Exact(vec![]), TypeSignature::VariadicAny],
                    Volatility::Immutable,
                ),
                PlanningFunction::Map(MapScalarFunction::Access)
                | PlanningFunction::Element
                | PlanningFunction::StructField => Signature::any(2, Volatility::Immutable),
                PlanningFunction::Map(MapScalarFunction::Concat) => {
                    Signature::variadic_any(Volatility::Immutable)
                }
            },
        }));
    }
}
#[derive(Debug, PartialEq, Eq)]
struct CollectionPlanningFunction {
    name: &'static str,
    function: PlanningFunction,
    signature: Signature,
}
// `function` is determined by `name` at registration, so hashing the name and
// signature agrees with the derived `Eq`.
impl Hash for CollectionPlanningFunction {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.signature.hash(state);
    }
}
impl CollectionPlanningFunction {
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
            PlanningFunction::Element | PlanningFunction::StructField
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
                PlanningFunction::Element => element_access_type(&args, &schema),
                PlanningFunction::StructField => struct_field_type(&args, &schema),
                PlanningFunction::Map(_) => unreachable!(),
            }
        } else if let PlanningFunction::Map(function) = self.function {
            function.output_type(&inputs)
        } else {
            unreachable!()
        }
        .map_err(DataFusionError::Plan)?;
        Ok((dtype_to_arrow(&dtype), nullable))
    }
}
impl ScalarUDFImpl for CollectionPlanningFunction {
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
        Err(DataFusionError::NotImplemented("collection planning adapter cannot execute; use a capable query engine or external exact sub-DAG".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn planning_adapter_explicitly_refuses_physical_execution() {
        let adapter = CollectionPlanningFunction {
            name: MAP_PLANNING_NAME,
            function: PlanningFunction::Map(MapScalarFunction::Construct),
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
            adapter.invoke_with_args(args),
            Err(DataFusionError::NotImplemented(_))
        ));
        let result = adapter.return_type(&[]).unwrap();
        let (expected, _) = MapScalarFunction::Construct.output_type(&[]).unwrap();
        assert_eq!(result, dtype_to_arrow(&expected));
    }
}
