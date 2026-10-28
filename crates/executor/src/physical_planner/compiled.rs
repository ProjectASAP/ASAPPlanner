//! Reader-independent physical computation and checked deployment instantiation.
use super::*;
use crate::sources::{BindingDataSources, Scan};

#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
enum Node {
    /// A materialized view slot: data bound by node id at run time, with this schema.
    Input(SchemaRef),
    /// A table slot: a `Scan` leaf, bound by its `Source`.
    Table(TableSlot),
    Operator {
        inputs: Vec<NodeId>,
        operator: Operator,
    },
}

/// A `Scan` leaf, computed once at compile time: what [`CompiledPhysicalDAG::instantiate`]
/// turns into a `Scan` over the provider bound to `source`.
#[derive(Clone)]
#[cfg_attr(any(test, feature = "test-support"), derive(serde::Serialize))]
pub(super) struct TableSlot {
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
    /// The window of each node that reads one range window directly (an
    /// expanded pane reading `TimeRange(w)` over its shifts), for when the
    /// node is a materialized view slot.
    materialized_view_windows: BTreeMap<NodeId, ScanWindow>,
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
            if fragment.input_slots().any(|(id, _)| id == *root) {
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
            if inputs.len() != fragment.input_slots().count() {
                return Err(invalid("physical fragment input arity mismatch"));
            }
            let mut mapping = BTreeMap::new();
            for ((local, expected), global) in fragment.input_slots().zip(inputs) {
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
            materialized_view_windows: BTreeMap::new(),
        }
    }
    pub(super) fn add_input(&mut self, id: NodeId, schema: SchemaRef) -> Result<(), Error> {
        self.insert(id, Node::Input(schema))
    }
    pub(super) fn add_table(&mut self, id: NodeId, slot: TableSlot) -> Result<(), Error> {
        self.insert(id, Node::Table(slot))
    }
    pub(super) fn set_materialized_view_window(&mut self, id: NodeId, window: ScanWindow) {
        self.materialized_view_windows.insert(id, window);
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
            Node::Input(_) | Node::Table(_) => Some(id),
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
            Node::Input(_) | Node::Table(_) => None,
        }
    }
    pub fn operator_name(&self, id: NodeId) -> Option<&str> {
        match self.nodes.get(&id)? {
            Node::Input(_) => Some("Input"),
            Node::Table(_) => Some("Scan"),
            Node::Operator { operator, .. } => Some(operator.name()),
        }
    }

    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }
    /// Each input slot (table and view) and its schema.
    pub fn input_slots(&self) -> impl Iterator<Item = (NodeId, &SchemaRef)> {
        self.nodes.iter().filter_map(|(&id, node)| match node {
            Node::Input(schema) | Node::Table(TableSlot { schema, .. }) => Some((id, schema)),
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
            Node::Input(schema) | Node::Table(TableSlot { schema, .. }) => Ok(schema.clone()),
            Node::Operator { operator, .. } => Ok(operator.output_schema()),
        }
    }
    /// Properties of every reachable node, derived in one schema-only pass.
    pub(super) fn output_properties(&self) -> Result<BTreeMap<NodeId, PlanProperties>, Error> {
        self.unbound()?.properties(&self.roots)
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
    /// each node in `boundaries` by a materialized view slot (a table slot stays one).
    /// Nothing is lowered again.
    pub(super) fn cut(
        &self,
        boundaries: &BTreeSet<NodeId>,
        roots: &[NodeId],
    ) -> Result<Self, Error> {
        let mut result = Self::new(roots.to_vec());
        let mut pending = roots.to_vec();
        while let Some(id) = pending.pop() {
            if result.nodes.contains_key(&id) {
                continue;
            }
            let node = self
                .nodes
                .get(&id)
                .ok_or_else(|| invalid(format!("missing physical node {id}")))?;
            let node = match (boundaries.contains(&id), node) {
                (true, Node::Operator { .. }) => Node::Input(self.output_schema(id)?),
                _ => node.clone(),
            };
            if let Node::Operator { inputs, .. } = &node {
                pending.extend(inputs);
            }
            if let Some(&window) = self.materialized_view_windows.get(&id) {
                result.materialized_view_windows.insert(id, window);
            }
            result.nodes.insert(id, node);
        }
        result.validate()?;
        Ok(result)
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
                Node::Input(schema) | Node::Table(TableSlot { schema, .. }) => {
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
    /// Bind every input slot and validate before any provider is read: a
    /// table slot to the provider of its `Source` in `data_sources.tables`,
    /// a materialized view slot to its node's provider in `data_sources.materialized_views`. Each
    /// provider must contain its slot's columns and declare `Bounded`.
    /// Opens no reader; each slot becomes a `Scan` that requests its rows
    /// when the run starts.
    pub fn instantiate<'a>(
        &self,
        data_sources: &'a BindingDataSources,
    ) -> Result<PhysicalDAG<'a, Batch, SchemaRef>, Error> {
        let mut dag = PhysicalDAG::default();
        for (&id, node) in &self.nodes {
            match node {
                Node::Input(schema) => {
                    let provider = data_sources
                        .materialized_views
                        .get(&id)
                        .ok_or_else(|| invalid(format!("missing physical input {id}")))?;
                    let scan = Scan::materialized_view(
                        provider.clone(),
                        schema.clone(),
                        self.materialized_view_windows
                            .get(&id)
                            .copied()
                            .unwrap_or(ScanWindow::Unbounded),
                    )
                    .map_err(|error| at_slot(id, error))?;
                    dag.add(id, vec![], scan)?;
                }
                Node::Table(slot) => {
                    let provider = data_sources
                        .tables
                        .provider(&slot.source)
                        .ok_or_else(|| invalid(format!("unbound raw source: {:?}", slot.source)))?;
                    let scan = Scan::table(
                        provider.clone(),
                        slot.schema.clone(),
                        slot.predicates.clone(),
                        slot.window,
                    )
                    .map_err(|error| at_slot(id, error))?;
                    dag.add(id, vec![], scan)?;
                }
                Node::Operator { inputs, operator } => {
                    dag.add(id, inputs.clone(), operator.clone())?;
                }
            }
        }
        if let Some(id) = data_sources
            .materialized_views
            .keys()
            .find(|id| !matches!(self.nodes.get(id), Some(Node::Input(_))))
        {
            return Err(invalid(format!(
                "materialized view binding {id} names no materialized view slot of this DAG"
            )));
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
