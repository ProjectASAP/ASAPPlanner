use crate::plan::NodeId;
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid DAG: {0}")]
    Invalid(String),
    #[error("operator failed: {0}")]
    Operator(String),
    #[error("node {node} ({operation}) failed: {source}")]
    AtNode {
        node: NodeId,
        operation: String,
        source: Box<Error>,
    },
    #[error("execution memory limit exceeded")]
    MemoryLimit,
    #[error("execution cancelled")]
    Cancelled,
}
