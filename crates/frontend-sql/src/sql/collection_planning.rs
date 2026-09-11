//! DataFusion planning adapters. Types come from the canonical signature rules;
//! physical evaluation deliberately remains the query engine's responsibility.
use super::types::{arrow_to_dtype, dtype_to_arrow, scalar_value_to_asap};
use asap_types::pre_asap::scalar_signature::{
    element_access_type, struct_field_type, MapScalarFunction,
};
use asap_types::pre_asap::{Column, QueryExpr, Schema};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::{DataFusionError, ExprSchema, Result};
use datafusion::logical_expr::{
    ColumnarValue, Expr, ExprSchemable, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
    Volatility,
};
use datafusion::prelude::SessionContext;

#[derive(Debug, Clone, Copy)]
enum PlanningFunction {
    Map(MapScalarFunction),
    Element,
    StructField,
}

pub(super) fn register(context: &SessionContext) {
    for (name, function) in [
        ("map", PlanningFunction::Map(MapScalarFunction::Construct)),
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
#[derive(Debug)]
struct CollectionPlanningFunction {
    name: &'static str,
    function: PlanningFunction,
    signature: Signature,
}
impl CollectionPlanningFunction {
    fn output(
        &self,
        args: &[DataType],
        nullable: &[bool],
        expressions: Option<&[Expr]>,
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
                        Column::new(format!("argument_{index}"), dtype, nullable)
                    })
                    .collect(),
            );
            let args = (0..schema.columns.len())
                .map(|index| {
                    if let Some(Expr::Literal(value)) = expressions.and_then(|args| args.get(index))
                    {
                        scalar_value_to_asap(value)
                            .map(QueryExpr::Literal)
                            .map_err(|error| DataFusionError::Plan(error.to_string()))
                    } else {
                        Ok(QueryExpr::Column(index))
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
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
    fn return_type_from_exprs(
        &self,
        args: &[Expr],
        schema: &dyn ExprSchema,
        types: &[DataType],
    ) -> Result<DataType> {
        let nullable = args
            .iter()
            .map(|arg| arg.nullable(schema))
            .collect::<Result<Vec<_>>>()?;
        self.output(types, &nullable, Some(args))
            .map(|output| output.0)
    }
    fn is_nullable(&self, args: &[Expr], schema: &dyn ExprSchema) -> bool {
        let types = args
            .iter()
            .map(|arg| arg.get_type(schema))
            .collect::<Result<Vec<_>>>();
        let nullable = args
            .iter()
            .map(|arg| arg.nullable(schema))
            .collect::<Result<Vec<_>>>();
        match (types, nullable) {
            (Ok(types), Ok(nullable)) => self
                .output(&types, &nullable, Some(args))
                .map(|out| out.1)
                .unwrap_or(true),
            _ => true,
        }
    }
    fn invoke_batch(&self, _args: &[ColumnarValue], _number_rows: usize) -> Result<ColumnarValue> {
        Err(DataFusionError::NotImplemented("collection planning adapter cannot execute; use a capable query engine or external exact subtree".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn planning_adapter_explicitly_refuses_physical_execution() {
        let adapter = CollectionPlanningFunction {
            name: "map",
            function: PlanningFunction::Map(MapScalarFunction::Construct),
            signature: Signature::any(0, Volatility::Immutable),
        };
        assert!(matches!(
            adapter.invoke_batch(&[], 1),
            Err(DataFusionError::NotImplemented(_))
        ));
        let result = adapter.return_type(&[]).unwrap();
        let (expected, _) = MapScalarFunction::Construct.output_type(&[]).unwrap();
        assert_eq!(result, dtype_to_arrow(&expected));
    }
}
