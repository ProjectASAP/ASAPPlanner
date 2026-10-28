//! Physical scalar/vector contracts preserve complete label sets across native computation.
use super::*;

pub fn scalar_schema() -> SchemaRef {
    crate::operators::vector_binary::value_schema(true)
}
pub fn vector_schema() -> SchemaRef {
    crate::operators::vector_binary::value_schema(false)
}

pub fn matrix_schema() -> SchemaRef {
    crate::operators::vector_window::matrix_schema()
}

pub fn compile_scalar(value: f64) -> Result<CompiledPhysicalDAG, Error> {
    let operator = Operator::scalar(
        crate::values::Value::Float64(value),
        planner_types::ir::schema::DataType::Float64,
    )?
    .with_output_schema(scalar_schema())?;
    CompiledPhysicalDAG::from_operators(BTreeMap::from([(0, (vec![], operator))]), vec![0])
}
