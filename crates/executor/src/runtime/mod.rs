//! Per-run producers run by each node's `Lifetime`, streams and resource ownership.
use crate::{
    plan::{Node, NodeId, PhysicalDAG},
    Error,
};
use futures::{stream::LocalBoxStream, Stream, TryStreamExt};
use planner_types::stages::Lifetime;
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    fmt::Debug,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll, Waker},
};
mod context;
pub use context::{Limits, Reservation, RunContext, Scope};
pub type OutputStream<'a, V> = LocalBoxStream<'a, Result<V, Error>>;
/// Every root's batches of an ingestion run, in the order of its roots.
pub type IngestionOutputs<V> = Result<Vec<Vec<SharedValue<V>>>, Error>;
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

/// A query-time run: one output stream per root. Fails before any operator
/// starts when a computed node that is not materialized
/// ([`Lifetime::None`]) has more than one consumer.
pub(crate) fn execute<'r, V: 'r, S: Clone + PartialEq + Debug + 'r>(
    dag: &'r PhysicalDAG<'_, V, S>,
    roots: &[NodeId],
    context: RunContext,
) -> Result<Vec<Input<'r, V>>, Error> {
    if context.evaluation_time_ms().is_none() {
        return Err(Error::Invalid(
            "an ingestion run is executed by `execute_ingestion`".into(),
        ));
    }
    start(dag, roots, context)
}

/// An ingestion run: every root's batches, the outputs the caller writes.
/// The runtime polls the roots together, so a node feeding several of them
/// is computed once and each batch reaches every consumer.
pub(crate) fn execute_ingestion<'r, V: 'r, S: Clone + PartialEq + Debug + 'r>(
    dag: &'r PhysicalDAG<'_, V, S>,
    roots: &[NodeId],
    context: RunContext,
) -> Result<impl std::future::Future<Output = IngestionOutputs<V>> + 'r, Error> {
    if context.evaluation_time_ms().is_some() {
        return Err(Error::Invalid(
            "a query run is executed by `execute`".into(),
        ));
    }
    let outputs = start(dag, roots, context)?;
    Ok(futures::future::try_join_all(
        outputs.into_iter().map(TryStreamExt::try_collect),
    ))
}

/// A node runs once per time it is evaluated at: an `@` anchor above it
/// evaluates it at another time than the run's.
type ProducerKey = (NodeId, Option<i64>);

/// Whether each consumer of `node` reads it on its own: at query time, a
/// leaf (a store read, a raw data or materialized view slot) is read once
/// per consumer, as the planner charges a not-materialized read per edge.
fn read_per_consumer<V, S>(node: &Node<'_, V, S>, context: &RunContext) -> bool {
    node.inputs.is_empty() && context.evaluation_time_ms().is_some()
}

fn start<'r, V: 'r, S: Clone + PartialEq + Debug + 'r>(
    dag: &'r PhysicalDAG<'_, V, S>,
    roots: &[NodeId],
    context: RunContext,
) -> Result<Vec<Input<'r, V>>, Error> {
    if context.is_cancelled() {
        return Err(Error::Cancelled);
    }
    dag.validate(roots)?;
    let mut pending = roots.to_vec();
    let mut visited = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if visited.insert(id) {
            let node = &dag.nodes[&id];
            node.operator.validate_context(&context)?;
            pending.extend(node.inputs.iter().copied());
        }
    }
    fn input_context<V, S>(
        dag: &PhysicalDAG<'_, V, S>,
        id: NodeId,
        context: &RunContext,
    ) -> Result<RunContext, Error> {
        let node = &dag.nodes[&id];
        node.operator
            .input_context(context)
            .map_err(|source| Error::AtNode {
                node: id,
                operation: node.operator.name().into(),
                source: Box::new(source),
            })
    }
    // The consumers of each shared producer, counted before any operator
    // starts.
    fn count_consumers<V, S>(
        dag: &PhysicalDAG<'_, V, S>,
        id: NodeId,
        context: &RunContext,
        consumers: &mut BTreeMap<ProducerKey, usize>,
    ) -> Result<(), Error> {
        let count = consumers
            .entry((id, context.evaluation_time_ms()))
            .or_default();
        *count += 1;
        if *count == 1 {
            let input_context = input_context(dag, id, context)?;
            for &child in &dag.nodes[&id].inputs {
                count_consumers(dag, child, &input_context, consumers)?;
            }
        }
        Ok(())
    }
    let mut consumers = BTreeMap::new();
    for &root in roots {
        count_consumers(dag, root, &context, &mut consumers)?;
    }
    if context.evaluation_time_ms().is_some() {
        for (&(id, _), &count) in &consumers {
            let node = &dag.nodes[&id];
            if count > 1 && !node.inputs.is_empty() && node.lifetime == Lifetime::None {
                return Err(Error::Invalid(format!(
                    "node {id} ({}) is not materialized but has {count} consumers at query time",
                    node.operator.name()
                )));
            }
        }
    }
    fn build<'r, V: 'r, S: 'r>(
        dag: &'r PhysicalDAG<'_, V, S>,
        id: NodeId,
        context: &RunContext,
        states: &mut BTreeMap<ProducerKey, Rc<RefCell<Producer<'r, V>>>>,
    ) -> Result<Rc<RefCell<Producer<'r, V>>>, Error> {
        let key = (id, context.evaluation_time_ms());
        let node = &dag.nodes[&id];
        let shared = !read_per_consumer(node, context);
        if let Some(state) = states.get(&key).filter(|_| shared) {
            return Ok(Rc::clone(state));
        }
        let input_context = input_context(dag, id, context)?;
        let mut inputs = Vec::new();
        for &child in &node.inputs {
            inputs.push(Input::subscribe(build(dag, child, &input_context, states)?));
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
            hold_until_finished: node.lifetime != Lifetime::None && !node.inputs.is_empty(),
            unread: BTreeMap::new(),
            next_reader: 0,
            batches_polled: 0,
            waiters: BTreeMap::new(),
            finished: false,
            failure: None,
        }));
        if shared {
            states.insert(key, Rc::clone(&state));
        }
        Ok(state)
    }
    let mut states = BTreeMap::new();
    roots
        .iter()
        .map(|&id| build(dag, id, &context, &mut states).map(Input::subscribe))
        .collect()
}
/// One run of a node, read by every consumer it has in the run.
struct Producer<'a, V> {
    node: NodeId,
    operation: String,
    stream: Option<OutputStream<'a, V>>,
    size: Box<dyn Fn(&V) -> usize + 'a>,
    context: RunContext,
    /// A materialized node ([`Lifetime::OneEvaluation`] or
    /// [`Lifetime::AcrossEvaluations`]) is computed to the end before any
    /// consumer reads it, and its batches stay charged to the run until
    /// every consumer has read them.
    hold_until_finished: bool,
    /// The batches each reader has not read yet, by reader.
    unread: BTreeMap<u64, VecDeque<SharedValue<V>>>,
    next_reader: u64,
    batches_polled: usize,
    /// The readers waiting for the next batch or the end.
    waiters: BTreeMap<u64, Waker>,
    finished: bool,
    failure: Option<Error>,
}
impl<V> Producer<'_, V> {
    fn wake_all(&mut self) {
        for (_, waker) in std::mem::take(&mut self.waiters) {
            waker.wake();
        }
    }
    fn finish(&mut self, failure: Option<Error>) {
        self.failure = failure;
        self.finished = true;
        self.stream = None;
        self.wake_all();
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
            state.unread.insert(id, VecDeque::new());
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
        state.unread.remove(&self.reader);
        state.waiters.remove(&self.reader);
        if state.unread.is_empty() {
            state.stream = None;
        }
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
        loop {
            if state.context.is_cancelled() && !state.finished {
                state.finish(Some(Error::Cancelled));
                for unread in state.unread.values_mut() {
                    unread.clear();
                }
            }
            if state.finished || !state.hold_until_finished {
                let unread = state
                    .unread
                    .get_mut(&this.reader)
                    .expect("subscribed reader");
                if let Some(value) = unread.pop_front() {
                    return Poll::Ready(Some(Ok(value)));
                }
            }
            if state.finished {
                this.done = true;
                state.unread.remove(&this.reader);
                return Poll::Ready(state.failure.clone().map(Err));
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
            match polled {
                Poll::Pending => {
                    // Any reader's progress wakes this one: the upstream
                    // wakes only the reader that polled it last.
                    state.waiters.insert(this.reader, cx.waker().clone());
                    return Poll::Pending;
                }
                Poll::Ready(Some(Ok(value))) => {
                    state.batches_polled += 1;
                    match state.context.reserve((state.size)(&value)) {
                        Ok(reservation) => {
                            let value = SharedValue {
                                value: Arc::new(value),
                                _reservation: Rc::new(reservation),
                            };
                            for unread in state.unread.values_mut() {
                                unread.push_back(value.clone());
                            }
                            // A held output is read only once it has
                            // finished, which wakes every reader.
                            if !state.hold_until_finished {
                                state.wake_all();
                            }
                        }
                        Err(error) => state.finish(Some(error)),
                    }
                }
                Poll::Ready(result) => {
                    let error = result.and_then(Result::err).map(|source| match source {
                        Error::AtNode { .. } | Error::Cancelled | Error::MemoryLimit => source,
                        source => Error::AtNode {
                            node: state.node,
                            operation: state.operation.clone(),
                            source: Box::new(source),
                        },
                    });
                    state.finish(error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

mod cooperative;
pub(crate) use cooperative::Cooperative;
