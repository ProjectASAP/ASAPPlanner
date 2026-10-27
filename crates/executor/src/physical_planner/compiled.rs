//! Reader-independent physical computation and checked deployment instantiation.
use super::*;

#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
enum Node {
    /// An input slot: data bound at run time with this schema.
    Input(SchemaRef),
    Operator {
        inputs: Vec<NodeId>,
        operator: Operator,
    },
}

/// Selected native operators and input slots. Rebinding never repeats lowering.
/// Derived inside the executor and never persisted; tests serialize it only
/// to inspect the selected operators.
#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
pub struct CompiledPhysicalDAG {
    nodes: BTreeMap<NodeId, Node>,
    roots: Vec<NodeId>,
}

impl CompiledPhysicalDAG {
    /// Link already-selected physical fragments without lowering operators again.
    /// Fragment keys and source keys share a namespace; repeated dependency IDs
    /// therefore remain one producer in the composed DAG.
    pub fn compose(
        sources: BTreeMap<NodeId, SchemaRef>,
        fragments: BTreeMap<NodeId, (Vec<NodeId>, Self)>,
        roots: Vec<NodeId>,
    ) -> Result<Self, Error> {
        if sources.keys().any(|id| fragments.contains_key(id)) {
            return Err(invalid("physical source and fragment IDs overlap"));
        }
        let mut schemas = sources.clone();
        for (&id, (_, fragment)) in &fragments {
            fragment.validate()?;
            let [root] = fragment.roots() else {
                return Err(invalid("composed fragment requires one root"));
            };
            if fragment.input_schemas().any(|(id, _)| id == *root) {
                return Err(invalid("fragment root must be a computed output"));
            }
            schemas.insert(id, fragment.output_schema(*root)?);
        }
        let mut next = schemas
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("physical node ID overflow"))?;
        let mut result = Self::new(roots);
        for (id, schema) in sources {
            result.add_input(id, schema)?;
        }
        for (id, (inputs, fragment)) in fragments {
            if inputs.len() != fragment.input_schemas().count() {
                return Err(invalid("physical fragment input arity mismatch"));
            }
            let mut mapping = BTreeMap::new();
            for ((local, expected), global) in fragment.input_schemas().zip(inputs) {
                let actual = schemas
                    .get(&global)
                    .ok_or_else(|| invalid("missing physical fragment dependency"))?;
                if expected != actual {
                    return Err(invalid("physical fragment dependency schema mismatch"));
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
        inputs: BTreeMap<NodeId, SchemaRef>,
        operators: BTreeMap<NodeId, (Vec<NodeId>, Operator)>,
        roots: Vec<NodeId>,
    ) -> Result<Self, Error> {
        let mut result = Self::new(roots);
        for (id, schema) in inputs {
            result.add_input(id, schema)?;
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
    pub(super) fn add_input(&mut self, id: NodeId, schema: SchemaRef) -> Result<(), Error> {
        self.insert(id, Node::Input(schema))
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
    pub fn input_schemas(&self) -> impl Iterator<Item = (NodeId, &SchemaRef)> {
        self.nodes.iter().filter_map(|(&id, node)| match node {
            Node::Input(schema) => Some((id, schema)),
            Node::Operator { .. } => None,
        })
    }
    /// The schema of a node's output, without opening deployment readers.
    pub fn output_schema(&self, id: NodeId) -> Result<SchemaRef, Error> {
        match self
            .nodes
            .get(&id)
            .ok_or_else(|| invalid("missing output"))?
        {
            Node::Input(schema) => Ok(schema.clone()),
            Node::Operator { operator, .. } => Ok(operator.output_schema()),
        }
    }
    /// Properties of every reachable node, derived in one schema-only pass.
    pub(super) fn output_properties(&self) -> Result<BTreeMap<NodeId, PlanProperties>, Error> {
        self.instantiate(self.unbound_inputs())?
            .properties(&self.roots)
    }
    /// A reader-free bounded stand-in for every input slot.
    fn unbound_inputs(&self) -> BTreeMap<NodeId, Source<'_>> {
        self.input_schemas()
            .map(|(id, schema)| (id, Box::new(UnboundInput(schema.clone())) as Source<'_>))
            .collect()
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
        boundaries: &BTreeMap<NodeId, SchemaRef>,
        roots: &[NodeId],
    ) -> Result<Self, Error> {
        let mut result = Self::new(roots.to_vec());
        let mut pending = roots.to_vec();
        while let Some(id) = pending.pop() {
            if result.nodes.contains_key(&id) {
                continue;
            }
            let node = match boundaries.get(&id) {
                Some(schema) => Node::Input(schema.clone()),
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
    /// Validate using schema-only sources. No deployment reader is available.
    pub fn validate(&self) -> Result<(), Error> {
        self.instantiate(self.unbound_inputs()).map(|_| ())
    }
    /// Resolve exactly the declared inputs and validate before any source starts.
    /// Every source must have the slot's schema and declare `Bounded`.
    pub fn instantiate<'a>(
        &self,
        mut sources: BTreeMap<NodeId, Source<'a>>,
    ) -> Result<PhysicalDAG<'a, Batch, SchemaRef>, Error> {
        let mut dag = PhysicalDAG::default();
        for (&id, node) in &self.nodes {
            match node {
                Node::Input(schema) => {
                    let source = sources
                        .remove(&id)
                        .ok_or_else(|| invalid(format!("missing physical input {id}")))?;
                    if !source.input_schemas().is_empty()
                        || source.output_schema() != *schema
                        || source.properties(&[]).boundedness != Boundedness::Bounded
                    {
                        return Err(invalid(format!(
                            "physical input {id} needs a bounded source with the slot's schema"
                        )));
                    }
                    dag.add_boxed(
                        id,
                        vec![],
                        Box::new(CheckedSource {
                            source,
                            output: schema.clone(),
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
/// Stands in for an input slot when validating or deriving properties.
struct UnboundInput(SchemaRef);
impl PhysicalOperator<Batch, SchemaRef> for UnboundInput {
    fn name(&self) -> &str {
        "UnresolvedInput"
    }
    fn input_schemas(&self) -> Vec<SchemaRef> {
        vec![]
    }
    fn output_schema(&self) -> SchemaRef {
        self.0.clone()
    }
    fn properties(&self, _: &[PlanProperties]) -> PlanProperties {
        PlanProperties {
            boundedness: Boundedness::Bounded,
            emission: Emission::Unknown,
        }
    }
    fn output_bytes(&self, batch: &Batch) -> usize {
        batch.memory_bytes()
    }
    fn start<'a>(
        &'a self,
        _: Vec<crate::runtime::Input<'a, Batch>>,
        _: crate::runtime::RunContext,
    ) -> Result<crate::runtime::OutputStream<'a, Batch>, Error> {
        Err(invalid("physical input must be resolved before execution"))
    }
}
