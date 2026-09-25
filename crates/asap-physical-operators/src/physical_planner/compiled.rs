//! Reader-independent physical computation and checked deployment instantiation.
use super::*;

/// A typed execution boundary, without storage identity or a live reader.
#[derive(Clone, Debug)]
pub struct InputContract {
    pub schema: Schema,
    pub properties: PlanProperties,
}
impl InputContract {
    pub fn bounded(schema: Schema) -> Self {
        Self {
            schema,
            properties: PlanProperties {
                boundedness: Boundedness::Bounded,
                emission: Emission::Unknown,
            },
        }
    }
    pub fn from_source(source: &dyn PhysicalOperator<Batch, Schema>) -> Self {
        Self {
            schema: source.output_schema(),
            properties: source.properties(&[]),
        }
    }
}
#[derive(Clone)]
enum Node {
    Input(InputContract),
    Operator {
        inputs: Vec<NodeId>,
        operator: Operator,
    },
}

/// Selected native operators and input slots. Rebinding never repeats lowering.
#[derive(Clone)]
pub struct CompiledPhysicalDag {
    nodes: BTreeMap<NodeId, Node>,
    roots: Vec<NodeId>,
}
impl CompiledPhysicalDag {
    pub(super) fn new(roots: Vec<NodeId>) -> Self {
        Self {
            nodes: BTreeMap::new(),
            roots,
        }
    }
    pub(super) fn add_input(&mut self, id: NodeId, contract: InputContract) -> Result<(), Error> {
        self.insert(id, Node::Input(contract))
    }
    pub(super) fn add(
        &mut self,
        id: NodeId,
        inputs: Vec<NodeId>,
        operator: Operator,
    ) -> Result<(), Error> {
        self.insert(id, Node::Operator { inputs, operator })
    }
    fn insert(&mut self, id: NodeId, node: Node) -> Result<(), Error> {
        if self.nodes.insert(id, node).is_some() {
            return Err(invalid(format!("duplicate physical node {id}")));
        }
        Ok(())
    }
    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }
    pub fn input_contracts(&self) -> impl Iterator<Item = (NodeId, &InputContract)> {
        self.nodes.iter().filter_map(|(&id, node)| match node {
            Node::Input(contract) => Some((id, contract)),
            Node::Operator { .. } => None,
        })
    }
    /// Validate using contract-only sources. No deployment reader is available.
    pub fn validate(&self) -> Result<(), Error> {
        let sources = self
            .input_contracts()
            .map(|(id, c)| (id, Box::new(c.clone()) as Source<'_>))
            .collect();
        self.instantiate(sources).map(|_| ())
    }
    /// Resolve exactly the declared inputs and validate before any source starts.
    pub fn instantiate<'a>(
        &self,
        mut sources: BTreeMap<NodeId, Source<'a>>,
    ) -> Result<PhysicalDag<'a, Batch, Schema>, Error> {
        let mut graph = PhysicalDag::default();
        for (&id, node) in &self.nodes {
            match node {
                Node::Input(contract) => {
                    let source = sources
                        .remove(&id)
                        .ok_or_else(|| invalid(format!("missing physical input {id}")))?;
                    let actual = source.properties(&[]);
                    if !source.input_schemas().is_empty()
                        || source.output_schema() != contract.schema
                        || (contract.properties.boundedness != Boundedness::Unknown
                            && actual.boundedness != contract.properties.boundedness)
                        || (contract.properties.emission != Emission::Unknown
                            && actual.emission != contract.properties.emission)
                    {
                        return Err(invalid(format!(
                            "physical input {id} violates its compiled contract"
                        )));
                    }
                    graph.add_boxed(
                        id,
                        vec![],
                        Box::new(CheckedSource {
                            source,
                            output: contract.schema.clone(),
                        }),
                    )?;
                }
                Node::Operator { inputs, operator } => {
                    graph.add(id, inputs.clone(), operator.clone())?;
                }
            }
        }
        if !sources.is_empty() {
            return Err(invalid("unexpected physical input binding"));
        }
        graph.validate(&self.roots)?;
        Ok(graph)
    }
}
impl PhysicalOperator<Batch, Schema> for InputContract {
    fn name(&self) -> &str {
        "UnresolvedInput"
    }
    fn input_schemas(&self) -> Vec<Schema> {
        vec![]
    }
    fn output_schema(&self) -> Schema {
        self.schema.clone()
    }
    fn properties(&self, _: &[PlanProperties]) -> PlanProperties {
        self.properties
    }
    fn output_bytes(&self, batch: &Batch) -> usize {
        batch.bytes()
    }
    fn start<'a>(
        &'a self,
        _: Vec<crate::runtime::Input<'a, Batch>>,
        _: crate::runtime::RunContext,
    ) -> Result<crate::runtime::OutputStream<'a, Batch>, Error> {
        Err(invalid("physical input must be resolved before execution"))
    }
}
