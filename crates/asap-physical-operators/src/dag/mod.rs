//! Independent operator DAG execution. No backend plan or engine types are used.
//!
//! Each run creates one stream per reachable node. Consumers subscribe to that
//! stream independently; retained outputs are released after the last consumer.
use futures::{stream::LocalBoxStream, Stream};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt::Debug,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll, Waker},
};

pub type NodeId = u64;
pub type OutputStream<'a, V> = LocalBoxStream<'a, Result<V, Error>>;
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

/// Scope is part of an execution instance, never mutable state in a reusable plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Ingestion {
        window_start_ms: i64,
        window_end_ms: i64,
        revision: u64,
    },
    Query {
        evaluation_time_ms: i64,
        revision: u64,
    },
}
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_buffered_batches: usize,
    pub max_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_buffered_batches: 8,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}
struct Control {
    cancelled: Cell<bool>,
    bytes: Cell<usize>,
    peak: Cell<usize>,
    limits: Limits,
    waiters: RefCell<Vec<Waker>>,
}
#[derive(Clone)]
pub struct RunContext {
    pub scope: Scope,
    control: Rc<Control>,
}
impl RunContext {
    pub fn new(scope: Scope, limits: Limits) -> Result<Self, Error> {
        if limits.max_buffered_batches == 0 || limits.max_bytes == 0 {
            return Err(Error::Invalid("execution limits must be positive".into()));
        }
        if matches!(&scope, Scope::Ingestion { window_start_ms, window_end_ms, .. } if window_start_ms > window_end_ms)
        {
            return Err(Error::Invalid("inverted ingestion window".into()));
        }
        Ok(Self {
            scope,
            control: Rc::new(Control {
                cancelled: Cell::new(false),
                bytes: Cell::new(0),
                peak: Cell::new(0),
                limits,
                waiters: RefCell::new(Vec::new()),
            }),
        })
    }
    pub fn cancel(&self) {
        self.control.cancelled.set(true);
        for waiter in self.control.waiters.borrow_mut().drain(..) {
            waiter.wake();
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.control.cancelled.get()
    }
    pub fn retained_bytes(&self) -> usize {
        self.control.bytes.get()
    }
    pub fn peak_bytes(&self) -> usize {
        self.control.peak.get()
    }
    pub fn reserve(&self, bytes: usize) -> Result<Reservation, Error> {
        let total = self
            .control
            .bytes
            .get()
            .checked_add(bytes)
            .ok_or(Error::MemoryLimit)?;
        if total > self.control.limits.max_bytes {
            return Err(Error::MemoryLimit);
        }
        self.control.bytes.set(total);
        self.control.peak.set(self.control.peak.get().max(total));
        Ok(Reservation {
            bytes,
            control: Rc::clone(&self.control),
        })
    }
    fn register(&self, waker: &Waker) {
        let mut waiters = self.control.waiters.borrow_mut();
        if !waiters.iter().any(|old| old.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }
}
pub struct Reservation {
    bytes: usize,
    control: Rc<Control>,
}
impl Reservation {
    /// Adjust an operator-owned allocation without accumulating bookkeeping entries.
    pub fn resize(&mut self, bytes: usize) -> Result<(), Error> {
        let total = self
            .control
            .bytes
            .get()
            .checked_sub(self.bytes)
            .and_then(|total| total.checked_add(bytes))
            .ok_or(Error::MemoryLimit)?;
        if total > self.control.limits.max_bytes {
            return Err(Error::MemoryLimit);
        }
        self.control.bytes.set(total);
        self.control.peak.set(self.control.peak.get().max(total));
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.control
            .bytes
            .set(self.control.bytes.get().saturating_sub(self.bytes));
    }
}

/// An output owns its memory reservation even after it leaves the DAG's queue.
pub struct SharedValue<V> {
    value: Arc<V>,
    _reservation: Rc<Reservation>,
}
impl<V> Clone for SharedValue<V> {
    fn clone(&self) -> Self {
        Self {
            value: Arc::clone(&self.value),
            _reservation: Rc::clone(&self._reservation),
        }
    }
}
impl<V> std::ops::Deref for SharedValue<V> {
    type Target = V;
    fn deref(&self) -> &V {
        &self.value
    }
}
impl<V> SharedValue<V> {
    pub fn value(&self) -> &V {
        &self.value
    }
}

/// Operators own computation. The runtime provides already-connected inputs;
/// an operator must not recursively execute another plan node itself.
pub trait PhysicalOperator<V, S> {
    fn name(&self) -> &str;
    fn input_schemas(&self) -> Vec<S>;
    fn output_schema(&self) -> S;
    fn start<'a>(
        &'a self,
        inputs: Vec<Input<'a, V>>,
        context: RunContext,
    ) -> Result<OutputStream<'a, V>, Error>;
    fn output_bytes(&self, value: &V) -> usize;
}
struct Node<'a, V, S> {
    inputs: Vec<NodeId>,
    operator: Box<dyn PhysicalOperator<V, S> + 'a>,
}
pub struct PhysicalDag<'a, V, S> {
    nodes: BTreeMap<NodeId, Node<'a, V, S>>,
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
        fn visit<V, S: Clone + PartialEq + Debug>(
            dag: &PhysicalDag<'_, V, S>,
            id: NodeId,
            active: &mut BTreeSet<NodeId>,
            done: &mut BTreeMap<NodeId, usize>,
        ) -> Result<usize, Error> {
            if let Some(depth) = done.get(&id) {
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
            for (input, schema) in node.inputs.iter().zip(expected) {
                depth = depth.max(1 + visit(dag, *input, active, done)?);
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
            active.remove(&id);
            done.insert(id, depth);
            Ok(depth)
        }
        if roots.is_empty() {
            return Err(Error::Invalid("execution needs a root".into()));
        }
        let mut done = BTreeMap::new();
        for &root in roots {
            visit(self, root, &mut BTreeSet::new(), &mut done)?;
        }
        Ok(())
    }
    pub fn execute<'r>(
        &'r self,
        roots: &[NodeId],
        context: RunContext,
    ) -> Result<Vec<Input<'r, V>>, Error>
    where
        'a: 'r,
    {
        if context.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.validate(roots)?;
        fn build<'r, V: 'r, S: 'r>(
            dag: &'r PhysicalDag<'_, V, S>,
            id: NodeId,
            context: &RunContext,
            states: &mut BTreeMap<NodeId, Rc<RefCell<Producer<'r, V>>>>,
        ) -> Result<Rc<RefCell<Producer<'r, V>>>, Error> {
            if let Some(state) = states.get(&id) {
                return Ok(Rc::clone(state));
            }
            let node = &dag.nodes[&id];
            let mut inputs = Vec::new();
            for &child in &node.inputs {
                inputs.push(Input::subscribe(build(dag, child, context, states)?));
            }
            let stream = node
                .operator
                .start(inputs, context.clone())
                .map_err(|source| Error::AtNode {
                    node: id,
                    operation: node.operator.name().into(),
                    source: Box::new(source),
                })?;
            let op = node.operator.as_ref();
            let state = Rc::new(RefCell::new(Producer {
                stream: Some(stream),
                node: id,
                operation: node.operator.name().into(),
                size: Box::new(move |value| op.output_bytes(value)),
                context: context.clone(),
                queue: VecDeque::new(),
                base: 0,
                next_reader: 0,
                batches_polled: 0,
                readers: BTreeMap::new(),
                waiters: BTreeMap::new(),
                finished: false,
                failure: None,
            }));
            states.insert(id, Rc::clone(&state));
            Ok(state)
        }
        let mut states = BTreeMap::new();
        roots
            .iter()
            .map(|&id| build(self, id, &context, &mut states).map(Input::subscribe))
            .collect()
    }
}
struct Producer<'a, V> {
    node: NodeId,
    operation: String,
    stream: Option<OutputStream<'a, V>>,
    size: Box<dyn Fn(&V) -> usize + 'a>,
    context: RunContext,
    queue: VecDeque<SharedValue<V>>,
    base: u64,
    next_reader: u64,
    batches_polled: usize,
    readers: BTreeMap<u64, u64>,
    waiters: BTreeMap<u64, Waker>,
    finished: bool,
    failure: Option<Error>,
}
impl<V> Producer<'_, V> {
    fn trim(&mut self) {
        let minimum = self
            .readers
            .values()
            .copied()
            .min()
            .unwrap_or(self.base + self.queue.len() as u64);
        while self.base < minimum {
            self.queue.pop_front();
            self.base += 1;
        }
        for (_, waker) in std::mem::take(&mut self.waiters) {
            waker.wake();
        }
        if self.readers.is_empty() {
            self.stream = None;
            self.queue.clear();
        }
    }
}
pub struct Input<'a, V> {
    producer: Rc<RefCell<Producer<'a, V>>>,
    reader: u64,
    done: bool,
}
impl<'a, V> Input<'a, V> {
    fn subscribe(producer: Rc<RefCell<Producer<'a, V>>>) -> Self {
        let reader = {
            let mut state = producer.borrow_mut();
            let id = state.next_reader;
            state.next_reader += 1;
            let base = state.base;
            state.readers.insert(id, base);
            id
        };
        Self {
            producer,
            reader,
            done: false,
        }
    }
}
impl<V> Drop for Input<'_, V> {
    fn drop(&mut self) {
        let mut state = self.producer.borrow_mut();
        state.readers.remove(&self.reader);
        state.waiters.remove(&self.reader);
        state.trim();
    }
}
impl<V> Stream for Input<'_, V> {
    type Item = Result<SharedValue<V>, Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let mut state = this.producer.borrow_mut();
        state.context.register(cx.waker());
        if state.context.is_cancelled() {
            state.failure = Some(Error::Cancelled);
            state.finished = true;
            state.stream = None;
            state.queue.clear();
        }
        let position = state.readers[&this.reader];
        let index = (position - state.base) as usize;
        if let Some(value) = state.queue.get(index).cloned() {
            state.readers.insert(this.reader, position + 1);
            state.trim();
            return Poll::Ready(Some(Ok(value)));
        }
        if state.finished {
            this.done = true;
            state.readers.remove(&this.reader);
            let failure = state.failure.clone();
            state.trim();
            return Poll::Ready(failure.map(Err));
        }
        state.waiters.insert(this.reader, cx.waker().clone());
        if state.queue.len() >= state.context.control.limits.max_buffered_batches {
            return Poll::Pending;
        }
        // Always-ready sources must still give cancellation and other roots a turn.
        if state.batches_polled >= 32 {
            state.batches_polled = 0;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let polled = state
            .stream
            .as_mut()
            .expect("unfinished producer")
            .as_mut()
            .poll_next(cx);
        if matches!(&polled, Poll::Ready(Some(Ok(_)))) {
            state.batches_polled += 1;
        }
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(value))) => match state.context.reserve((state.size)(&value)) {
                Ok(reservation) => {
                    let value = SharedValue {
                        value: Arc::new(value),
                        _reservation: Rc::new(reservation),
                    };
                    state.queue.push_back(value.clone());
                    state.readers.insert(this.reader, position + 1);
                    state.trim();
                    Poll::Ready(Some(Ok(value)))
                }
                Err(error) => {
                    state.failure = Some(error.clone());
                    state.finished = true;
                    state.stream = None;
                    this.done = true;
                    state.readers.remove(&this.reader);
                    state.trim();
                    Poll::Ready(Some(Err(error)))
                }
            },
            Poll::Ready(result) => {
                let error = result.and_then(Result::err).map(|source| match source {
                    Error::AtNode { .. } | Error::Cancelled | Error::MemoryLimit => source,
                    source => Error::AtNode {
                        node: state.node,
                        operation: state.operation.clone(),
                        source: Box::new(source),
                    },
                });
                state.failure = error.clone();
                state.finished = true;
                state.stream = None;
                this.done = true;
                state.readers.remove(&this.reader);
                state.trim();
                Poll::Ready(error.map(Err))
            }
        }
    }
}

pub mod operators;
pub mod values;

#[cfg(test)]
mod tests;

pub mod planner;

pub mod batch_execution;
