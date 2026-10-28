//! Reader-independent physical computation and checked deployment instantiation.
use super::*;
use crate::sources::{DataStore, Scan};
use planner_types::ir::materialized_view::MaterializedView;

#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
enum Node {
    /// A materialized view slot: the view's stored states, read by its
    /// `MaterializedView`.
    MaterializedView(MaterializedViewSlot),
    /// A raw data slot: a `Scan` leaf, read by its `Source`.
    RawData(RawDataSlot),
    Operator {
        inputs: Vec<NodeId>,
        operator: Operator,
    },
}

/// A materialized view the compiled DAG reads, computed once at compile time.
#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
pub(super) struct MaterializedViewSlot {
    pub(super) view: MaterializedView,
    /// The view node's schema.
    pub(super) schema: SchemaRef,
    /// The data its state summarizes ([`PhysicalASAPDAG::view_window`]).
    pub(super) window: ScanWindow,
}

/// A `Scan` leaf, computed once at compile time: what [`CompiledPhysicalDAG::instantiate`]
/// turns into a `Scan` reading `source` from the run's [`DataStore`].
#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
pub(super) struct RawDataSlot {
    pub(super) source: planner_types::ir::operator::Source,
    pub(super) schema: SchemaRef,
    /// The `Scan`'s predicates, compiled over `schema`.
    pub(super) predicates: Vec<crate::expressions::CompiledExpression>,
    /// The rows its readers read ([`PhysicalASAPDAG::scan_window`]).
    pub(super) window: ScanWindow,
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
    /// Assemble already-lowered operators; leaves read no data store (for
    /// example [`Operator::source`] holds its rows).
    pub fn from_operators(
        operators: BTreeMap<NodeId, (Vec<NodeId>, Operator)>,
        roots: Vec<NodeId>,
    ) -> Result<Self, Error> {
        let mut result = Self::new(roots);
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
    pub(super) fn add_materialized_view(
        &mut self,
        id: NodeId,
        slot: MaterializedViewSlot,
    ) -> Result<(), Error> {
        self.insert(id, Node::MaterializedView(slot))
    }
    pub(super) fn add_raw_data(&mut self, id: NodeId, slot: RawDataSlot) -> Result<(), Error> {
        self.insert(id, Node::RawData(slot))
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
            Node::MaterializedView(_) | Node::RawData(_) => Some(id),
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
            Node::MaterializedView(_) | Node::RawData(_) => None,
        }
    }
    pub fn operator_name(&self, id: NodeId) -> Option<&str> {
        match self.nodes.get(&id)? {
            Node::MaterializedView(_) => Some("MaterializedView"),
            Node::RawData(_) => Some("Scan"),
            Node::Operator { operator, .. } => Some(operator.name()),
        }
    }

    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }
    /// Each input slot (raw data and materialized view) and its schema.
    pub fn input_slots(&self) -> impl Iterator<Item = (NodeId, &SchemaRef)> {
        self.nodes.iter().filter_map(|(&id, node)| match node {
            Node::MaterializedView(MaterializedViewSlot { schema, .. })
            | Node::RawData(RawDataSlot { schema, .. }) => Some((id, schema)),
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
            Node::MaterializedView(MaterializedViewSlot { schema, .. })
            | Node::RawData(RawDataSlot { schema, .. }) => Ok(schema.clone()),
            Node::Operator { operator, .. } => Ok(operator.output_schema()),
        }
    }
    /// Validate using schema-only sources. No deployment reader is available.
    pub fn validate(&self) -> Result<(), Error> {
        self.unbound().map(|_| ())
    }
    /// The DAG with a reader-free bounded stand-in for every input slot.
    fn unbound(&self) -> Result<PhysicalDAG<'_, Batch, SchemaRef>, Error> {
        let mut dag = PhysicalDAG::default();
        for (&id, node) in &self.nodes {
            match node {
                Node::MaterializedView(MaterializedViewSlot { schema, .. })
                | Node::RawData(RawDataSlot { schema, .. }) => {
                    dag.add(id, vec![], UnboundInput(schema.clone()))?
                }
                Node::Operator { inputs, operator } => {
                    dag.add(id, inputs.clone(), operator.clone())?
                }
            }
        }
        dag.validate(&self.roots)?;
        Ok(dag)
    }
    /// Bind every input slot to `store` and validate. Opens no reader:
    /// each slot becomes a `Scan` that reads `store` when the run starts, a
    /// raw data slot by its `Source` and a materialized view slot by its
    /// `MaterializedView`.
    pub fn instantiate(
        &self,
        store: Arc<dyn DataStore>,
    ) -> Result<PhysicalDAG<'static, Batch, SchemaRef>, Error> {
        let mut dag = PhysicalDAG::default();
        for (&id, node) in &self.nodes {
            match node {
                Node::MaterializedView(slot) => {
                    let scan = Scan::materialized_view(
                        store.clone(),
                        slot.view.clone(),
                        slot.schema.clone(),
                        slot.window,
                    )
                    .map_err(|error| at_slot(id, error))?;
                    dag.add(id, vec![], scan)?;
                }
                Node::RawData(slot) => {
                    let scan = Scan::raw_data(
                        store.clone(),
                        slot.source.clone(),
                        slot.schema.clone(),
                        slot.predicates.clone(),
                        slot.window,
                    );
                    dag.add(id, vec![], scan)?;
                }
                Node::Operator { inputs, operator } => {
                    dag.add(id, inputs.clone(), operator.clone())?;
                }
            }
        }
        dag.validate(&self.roots)?;
        Ok(dag)
    }
}
/// `error` binding the slot `id`.
fn at_slot(id: NodeId, error: Error) -> Error {
    match error {
        Error::Invalid(message) => invalid(format!("physical input {id}: {message}")),
        other => other,
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
