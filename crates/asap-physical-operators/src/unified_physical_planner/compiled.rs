//! Reader-independent physical computation and checked deployment instantiation.
use super::*;

/// A typed execution boundary, without storage identity or a live reader.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct InputContract {
    pub schema: SchemaRef,
    pub properties: PlanProperties,
}
impl InputContract {
    pub fn bounded(schema: SchemaRef) -> Self {
        Self {
            schema,
            properties: PlanProperties {
                boundedness: Boundedness::Bounded,
                emission: Emission::Unknown,
            },
        }
    }
    pub fn from_source(source: &dyn PhysicalOperator<Batch, SchemaRef>) -> Self {
        Self {
            schema: source.output_schema(),
            properties: source.properties(&[]),
        }
    }
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum Node {
    Input(InputContract),
    Operator {
        inputs: Vec<NodeId>,
        operator: Operator,
    },
}

/// Selected native operators and input slots. Rebinding never repeats lowering.
/// Serde is format-agnostic; deployments choose the encoding and its versioning.
/// Deserialization validates the dag before it is usable.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "UncheckedDAG")]
pub struct CompiledPhysicalDAG {
    nodes: BTreeMap<NodeId, Node>,
    roots: Vec<NodeId>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedDAG {
    nodes: BTreeMap<NodeId, Node>,
    roots: Vec<NodeId>,
}
impl TryFrom<UncheckedDAG> for CompiledPhysicalDAG {
    type Error = Error;
    fn try_from(dag: UncheckedDAG) -> Result<Self, Error> {
        let result = Self {
            nodes: dag.nodes,
            roots: dag.roots,
        };
        result.validate()?;
        Ok(result)
    }
}

impl CompiledPhysicalDAG {
    /// Link already-selected physical fragments without lowering operators again.
    /// Fragment keys and source keys share a namespace; repeated dependency IDs
    /// therefore remain one producer in the composed dag.
    pub fn compose(
        sources: BTreeMap<NodeId, InputContract>,
        fragments: BTreeMap<NodeId, (Vec<NodeId>, Self)>,
        roots: Vec<NodeId>,
    ) -> Result<Self, Error> {
        if sources.keys().any(|id| fragments.contains_key(id)) {
            return Err(invalid("physical source and fragment IDs overlap"));
        }
        let mut contracts = sources.clone();
        for (&id, (_, fragment)) in &fragments {
            fragment.validate()?;
            let [root] = fragment.roots() else {
                return Err(invalid("composed fragment requires one root"));
            };
            if fragment.input_contracts().any(|(id, _)| id == *root) {
                return Err(invalid("fragment root must be a computed output"));
            }
            contracts.insert(id, fragment.output_contract(*root)?);
        }
        let mut next = contracts
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("physical node ID overflow"))?;
        let mut result = Self::new(roots);
        for (id, contract) in sources {
            result.add_input(id, contract)?;
        }
        for (id, (inputs, fragment)) in fragments {
            if inputs.len() != fragment.input_contracts().count() {
                return Err(invalid("physical fragment input arity mismatch"));
            }
            let mut mapping = BTreeMap::new();
            for ((local, expected), global) in fragment.input_contracts().zip(inputs) {
                let actual = contracts
                    .get(&global)
                    .ok_or_else(|| invalid("missing physical fragment dependency"))?;
                if expected.schema != actual.schema
                    || (expected.properties.boundedness == Boundedness::Bounded
                        && actual.properties.boundedness != Boundedness::Bounded)
                {
                    return Err(invalid("physical fragment dependency contract mismatch"));
                }
                mapping.insert(local, global);
            }
            mapping.insert(fragment.roots[0], id);
            for local in fragment.nodes.keys() {
                if !mapping.contains_key(local) {
                    mapping.insert(*local, next);
                    next = next
                        .checked_add(1)
                        .ok_or_else(|| invalid("physical node ID overflow"))?;
                }
            }
            for (local, node) in fragment.nodes {
                if let Node::Operator { inputs, operator } = node {
                    result.add(
                        mapping[&local],
                        inputs.into_iter().map(|input| mapping[&input]).collect(),
                        operator,
                    )?;
                }
            }
        }
        result.validate()?;
        Ok(result)
    }

    /// Assemble already-lowered operators and typed external inputs. This is
    /// useful for engines that compose multiple compiled computation fragments.
    pub fn from_operators(
        inputs: BTreeMap<NodeId, InputContract>,
        operators: BTreeMap<NodeId, (Vec<NodeId>, Operator)>,
        roots: Vec<NodeId>,
    ) -> Result<Self, Error> {
        let mut result = Self::new(roots);
        for (id, contract) in inputs {
            result.add_input(id, contract)?;
        }
        for (id, (inputs, operator)) in operators {
            result.add(id, inputs, operator)?;
        }
        result.validate()?;
        Ok(result)
    }
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
    /// Identify the external input whose rows survive unchanged at this output.
    /// Protocol adapters can retain labels that are outside a closed physical schema.
    pub fn row_source(&self, id: NodeId) -> Option<NodeId> {
        match self.nodes.get(&id)? {
            Node::Input(_) => Some(id),
            Node::Operator { inputs, operator } => {
                let index = operator.row_preserving_input()?;
                self.row_source(*inputs.get(index)?)
            }
        }
    }

    /// Selected operator name, for plan inspection without decoding its wire format.
    /// Certified candidate pruning checks authoritative-key coverage inside this operator.
    pub fn certified_pruning_keys(&self, id: NodeId) -> Option<&[(usize, usize)]> {
        match self.nodes.get(&id)? {
            Node::Operator { operator, .. } => operator.certified_pruning_keys(),
            Node::Input(_) => None,
        }
    }
    pub fn operator_name(&self, id: NodeId) -> Option<&str> {
        match self.nodes.get(&id)? {
            Node::Input(_) => Some("Input"),
            Node::Operator { operator, .. } => Some(operator.name()),
        }
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
    /// Derive a reachable output contract without opening deployment readers.
    pub fn output_contract(&self, id: NodeId) -> Result<InputContract, Error> {
        let properties = *self
            .output_properties()?
            .get(&id)
            .ok_or_else(|| invalid("output is not reachable"))?;
        let schema = match self
            .nodes
            .get(&id)
            .ok_or_else(|| invalid("missing output"))?
        {
            Node::Input(contract) => contract.schema.clone(),
            Node::Operator { operator, .. } => operator.output_schema(),
        };
        Ok(InputContract { schema, properties })
    }
    /// Properties of every reachable node, derived in one contract-only pass.
    pub(super) fn output_properties(&self) -> Result<BTreeMap<NodeId, PlanProperties>, Error> {
        let sources = self
            .input_contracts()
            .map(|(id, contract)| (id, Box::new(contract.clone()) as Source<'_>))
            .collect();
        self.instantiate(sources)?.properties(&self.roots)
    }
    /// Direct physical dependencies; empty for inputs and unknown IDs.
    pub(super) fn dependencies(&self, id: NodeId) -> &[NodeId] {
        match self.nodes.get(&id) {
            Some(Node::Operator { inputs, .. }) => inputs,
            _ => &[],
        }
    }
    pub(super) fn is_operator(&self, id: NodeId) -> bool {
        matches!(self.nodes.get(&id), Some(Node::Operator { .. }))
    }
    /// Keep the already-lowered operators reachable from `roots`, replacing
    /// each node in `boundaries` by a typed input. Nothing is lowered again.
    pub(super) fn cut(
        &self,
        boundaries: &BTreeMap<NodeId, InputContract>,
        roots: &[NodeId],
    ) -> Result<Self, Error> {
        let mut result = Self::new(roots.to_vec());
        let mut pending = roots.to_vec();
        while let Some(id) = pending.pop() {
            if result.nodes.contains_key(&id) {
                continue;
            }
            let node = match boundaries.get(&id) {
                Some(contract) => Node::Input(contract.clone()),
                None => self
                    .nodes
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| invalid(format!("missing physical node {id}")))?,
            };
            if let Node::Operator { inputs, .. } = &node {
                pending.extend(inputs);
            }
            result.nodes.insert(id, node);
        }
        result.validate()?;
        Ok(result)
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
    ) -> Result<PhysicalDAG<'a, Batch, SchemaRef>, Error> {
        let mut dag = PhysicalDAG::default();
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
                    dag.add_boxed(
                        id,
                        vec![],
                        Box::new(CheckedSource {
                            source,
                            output: contract.schema.clone(),
                        }),
                    )?;
                }
                Node::Operator { inputs, operator } => {
                    dag.add(id, inputs.clone(), operator.clone())?;
                }
            }
        }
        if !sources.is_empty() {
            return Err(invalid("unexpected physical input binding"));
        }
        dag.validate(&self.roots)?;
        Ok(dag)
    }
}
impl PhysicalOperator<Batch, SchemaRef> for InputContract {
    fn name(&self) -> &str {
        "UnresolvedInput"
    }
    fn input_schemas(&self) -> Vec<SchemaRef> {
        vec![]
    }
    fn output_schema(&self) -> SchemaRef {
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
