//! DataFusion planning adapters. Types come from the canonical signature rules;
//! physical evaluation deliberately remains the query engine's responsibility.
use super::types::{arrow_to_dtype, dtype_to_arrow};
use asap_types::pre_asap::scalar_signature::MapScalarFunction;
use datafusion::arrow::datatypes::DataType;
use datafusion::common::{DataFusionError, ExprSchema, Result};
use datafusion::logical_expr::{
    ColumnarValue, Expr, ExprSchemable, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
    Volatility,
};
use datafusion::prelude::SessionContext;

pub(super) fn register(context: &SessionContext) {
    for (name, function) in [
        ("map", MapScalarFunction::Construct),
        ("mapconcat", MapScalarFunction::Concat),
        ("arrayelement", MapScalarFunction::Access),
    ] {
        context.register_udf(ScalarUDF::from(MapPlanningFunction {
            name,
            function,
            signature: match function {
                MapScalarFunction::Construct => Signature::one_of(
                    vec![TypeSignature::Exact(vec![]), TypeSignature::VariadicAny],
                    Volatility::Immutable,
                ),
                MapScalarFunction::Access => Signature::any(2, Volatility::Immutable),
                MapScalarFunction::Concat => Signature::variadic_any(Volatility::Immutable),
            },
        }));
    }
}
#[derive(Debug)]
struct MapPlanningFunction {
    name: &'static str,
    function: MapScalarFunction,
    signature: Signature,
}
impl MapPlanningFunction {
    fn output(&self, args: &[DataType], nullable: &[bool]) -> Result<(DataType, bool)> {
        let inputs = args
            .iter()
            .zip(nullable)
            .map(|(dtype, null)| {
                arrow_to_dtype(dtype)
                    .map(|dtype| (dtype, *null))
                    .map_err(|e| DataFusionError::Plan(e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let (dtype, nullable) = self
            .function
            .output_type(&inputs)
            .map_err(DataFusionError::Plan)?;
        Ok((dtype_to_arrow(&dtype), nullable))
    }
}
impl ScalarUDFImpl for MapPlanningFunction {
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
        self.output(types, &nullable).map(|output| output.0)
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
                .output(&types, &nullable)
                .map(|out| out.1)
                .unwrap_or(true),
            _ => true,
        }
    }
    fn invoke_batch(&self, _args: &[ColumnarValue], _number_rows: usize) -> Result<ColumnarValue> {
        Err(DataFusionError::NotImplemented("map planning adapter cannot execute; use a capable query engine or external exact subtree".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn planning_adapter_explicitly_refuses_physical_execution() {
        let adapter = MapPlanningFunction {
            name: "map",
            function: MapScalarFunction::Construct,
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
