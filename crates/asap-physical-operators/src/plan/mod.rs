//! Immutable physical graph, operator contracts and pre-execution validation.
use crate::{
    runtime::{Input, OutputStream, RunContext},
    Error,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
};
pub type NodeId = u64;
mod properties;
pub use properties::{Boundedness, Emission, PlanProperties};
/// Operators own computation. The runtime provides already-connected inputs;
/// an operator must not recursively execute another plan node itself.
pub trait PhysicalOperator<V, S> {
    fn name(&self) -> &str;
    /// Source implementations must explicitly declare finite input before feeding blocking operators.
    fn properties(&self, inputs: &[PlanProperties]) -> PlanProperties {
        PlanProperties {
            boundedness: Boundedness::from_inputs(inputs),
            emission: Emission::Unknown,
        }
    }
    fn requires_bounded_input(&self) -> bool {
        false
    }

    /// Validate run-specific contracts before any source is opened.
    fn validate_context(&self, _context: &RunContext) -> Result<(), Error> {
        Ok(())
    }
    fn input_schemas(&self) -> Vec<S>;
    fn output_schema(&self) -> S;
    fn start<'a>(
        &'a self,
        inputs: Vec<Input<'a, V>>,
        context: RunContext,
    ) -> Result<OutputStream<'a, V>, Error>;
    fn output_bytes(&self, value: &V) -> usize;
}
pub(crate) struct Node<'a, V, S> {
    pub(crate) inputs: Vec<NodeId>,
    pub(crate) operator: Box<dyn PhysicalOperator<V, S> + 'a>,
}
pub struct PhysicalDag<'a, V, S> {
    pub(crate) nodes: BTreeMap<NodeId, Node<'a, V, S>>,
}
impl<V, S> Default for PhysicalDag<'_, V, S> {
    fn default() -> Self {
        Self {
            nodes: BTreeMap::new(),
        }
    }
}
impl<'a, V: 'a, S: Clone + PartialEq + Debug + 'a> PhysicalDag<'a, V, S> {
    pub fn add(
        &mut self,
        id: NodeId,
        inputs: Vec<NodeId>,
        operator: impl PhysicalOperator<V, S> + 'a,
    ) -> Result<(), Error> {
        self.add_boxed(id, inputs, Box::new(operator))
    }
    pub fn add_boxed(
        &mut self,
        id: NodeId,
        inputs: Vec<NodeId>,
        operator: Box<dyn PhysicalOperator<V, S> + 'a>,
    ) -> Result<(), Error> {
        if self.nodes.contains_key(&id) {
            return Err(Error::Invalid(format!("duplicate node {id}")));
        }
        self.nodes.insert(id, Node { inputs, operator });
        Ok(())
    }
    pub fn validate(&self, roots: &[NodeId]) -> Result<(), Error> {
        self.properties(roots).map(|_| ())
    }
    /// Derive properties while checking topology and schemas, before starting sources.
    pub fn properties(&self, roots: &[NodeId]) -> Result<BTreeMap<NodeId, PlanProperties>, Error> {
        fn visit<V, S: Clone + PartialEq + Debug>(
            dag: &PhysicalDag<'_, V, S>,
            id: NodeId,
            active: &mut BTreeSet<NodeId>,
            done: &mut BTreeMap<NodeId, (usize, PlanProperties)>,
        ) -> Result<usize, Error> {
            if let Some((depth, _)) = done.get(&id) {
                return Ok(*depth);
            }
            if active.len() >= 128 {
                return Err(Error::Invalid(
                    "DAG exceeds the supported execution depth of 128".into(),
                ));
            }
            if !active.insert(id) {
                return Err(Error::Invalid(format!("cycle at node {id}")));
            }
            let node = dag
                .nodes
                .get(&id)
                .ok_or_else(|| Error::Invalid(format!("missing node {id}")))?;
            let expected = node.operator.input_schemas();
            if expected.len() != node.inputs.len() {
                return Err(Error::Invalid(format!("node {id} input arity mismatch")));
            }
            let mut depth = 1;
            let mut input_properties = Vec::new();
            for (input, schema) in node.inputs.iter().zip(expected) {
                depth = depth.max(1 + visit(dag, *input, active, done)?);
                input_properties.push(done[input].1);
                let actual = dag.nodes[input].operator.output_schema();
                if actual != schema {
                    return Err(Error::Invalid(format!(
                        "node {id} input {input} schema mismatch: {actual:?} vs {schema:?}"
                    )));
                }
            }
            if depth > 128 {
                return Err(Error::Invalid(
                    "DAG exceeds the supported execution depth of 128".into(),
                ));
            }
            if node.operator.requires_bounded_input()
                && input_properties
                    .iter()
                    .any(|p| p.boundedness != Boundedness::Bounded)
            {
                return Err(Error::Invalid(format!(
                    "node {id} ({}) requires bounded inputs",
                    node.operator.name()
                )));
            }
            let properties = node.operator.properties(&input_properties);
            active.remove(&id);
            done.insert(id, (depth, properties));
            Ok(depth)
        }
        if roots.is_empty() {
            return Err(Error::Invalid("execution needs a root".into()));
        }
        let mut done = BTreeMap::new();
        for &root in roots {
            visit(self, root, &mut BTreeSet::new(), &mut done)?;
        }
        Ok(done
            .into_iter()
            .map(|(id, (_, properties))| (id, properties))
            .collect())
    }
    pub fn execute<'r>(
        &'r self,
        roots: &[NodeId],
        context: RunContext,
    ) -> Result<Vec<Input<'r, V>>, Error>
    where
        'a: 'r,
    {
        crate::runtime::execute(self, roots, context)
    }
}
