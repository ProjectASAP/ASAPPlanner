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
    rc::{Rc, Weak},
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

/// A query-time run: one output stream per root. Rejects an ingestion
/// scope, and a computed node with [`Lifetime::None`] and more than one
/// consumer (a planner bug), before any operator starts.
pub(crate) fn execute<'r, V: 'r, S: Clone + PartialEq + Debug + 'r>(
    dag: &'r PhysicalDAG<'_, V, S>,
    roots: &[NodeId],
    context: RunContext,
) -> Result<Vec<Input<'r, V>>, Error> {
    if context.evaluation_time_ms().is_none() {
        return Err(Error::Invalid(
            "`execute` takes a query scope; run an ingestion scope with `execute_ingestion`".into(),
        ));
    }
    start(dag, roots, context)
}

/// An ingestion run: every root's batches, the outputs the caller writes.
/// The runtime polls the roots together, so a node feeding several of them
/// is computed once and each batch reaches every consumer. Rejects a query
/// scope.
pub(crate) fn execute_ingestion<'r, V: 'r, S: Clone + PartialEq + Debug + 'r>(
    dag: &'r PhysicalDAG<'_, V, S>,
    roots: &[NodeId],
    context: RunContext,
) -> Result<impl std::future::Future<Output = IngestionOutputs<V>> + 'r, Error> {
    if context.evaluation_time_ms().is_some() {
        return Err(Error::Invalid(
            "`execute_ingestion` takes an ingestion scope; run a query scope with `execute`".into(),
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

/// How a node runs in a run, from the run's scope, whether it is a leaf and
/// its plan `Lifetime` (see the crate README).
#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeRun {
    /// Query time, a leaf (a raw data or materialized view slot): each
    /// consumer reads the store on its own, as the planner charges a
    /// not-materialized read on each edge.
    ReadPerConsumer,
    /// Query time, a materialized computed node: run to the end before any
    /// consumer reads it, then read by each consumer.
    Held,
    /// Every other node: one run whose batches reach each consumer as they
    /// come. At query time a computed one has a single consumer.
    Streamed,
}
fn run_as<V, S>(node: &Node<'_, V, S>, context: &RunContext) -> NodeRun {
    let query = context.evaluation_time_ms().is_some();
    match (query, node.inputs.is_empty(), node.lifetime) {
        (true, true, _) => NodeRun::ReadPerConsumer,
        (true, false, Lifetime::OneEvaluation(_) | Lifetime::AcrossEvaluations(_)) => NodeRun::Held,
        _ => NodeRun::Streamed,
    }
}

/// Each (node, evaluation time) a run reaches: its consumer count and the
/// context its inputs run in.
type Reached = BTreeMap<ProducerKey, (usize, RunContext)>;

/// Visit `id` once per evaluation time, before any operator starts:
/// validate its run context, count its consumers and record its inputs'
/// context.
fn reach<V, S>(
    dag: &PhysicalDAG<'_, V, S>,
    id: NodeId,
    context: &RunContext,
    reached: &mut Reached,
) -> Result<(), Error> {
    let key = (id, context.evaluation_time_ms());
    if let Some((count, _)) = reached.get_mut(&key) {
        *count += 1;
        return Ok(());
    }
    let node = &dag.nodes[&id];
    node.operator.validate_context(context)?;
    let input_context = node
        .operator
        .input_context(context)
        .map_err(|source| Error::AtNode {
            node: id,
            operation: node.operator.name().into(),
            source: Box::new(source),
        })?;
    for &child in &node.inputs {
        reach(dag, child, &input_context, reached)?;
    }
    reached.insert(key, (1, input_context));
    Ok(())
}

fn build<'r, V: 'r, S: 'r>(
    dag: &'r PhysicalDAG<'_, V, S>,
    id: NodeId,
    context: &RunContext,
    reached: &Reached,
    states: &mut BTreeMap<ProducerKey, Rc<RefCell<Producer<'r, V>>>>,
    run_producers: &RunProducers<'r, V>,
) -> Result<Rc<RefCell<Producer<'r, V>>>, Error> {
    let key = (id, context.evaluation_time_ms());
    let node = &dag.nodes[&id];
    let run = run_as(node, context);
    if run != NodeRun::ReadPerConsumer {
        if let Some(state) = states.get(&key) {
            return Ok(Rc::clone(state));
        }
    }
    let input_context = &reached[&key].1;
    let mut inputs = Vec::new();
    for &child in &node.inputs {
        inputs.push(Input::subscribe(build(
            dag,
            child,
            input_context,
            reached,
            states,
            run_producers,
        )?));
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
        hold_until_finished: run == NodeRun::Held,
        expected_readers: match run {
            NodeRun::ReadPerConsumer => 1,
            NodeRun::Held | NodeRun::Streamed => reached[&key].0,
        },
        run_producers: Rc::clone(run_producers),
        unread: BTreeMap::new(),
        next_reader: 0,
        batches_polled: 0,
        waiters: BTreeMap::new(),
        finished: false,
        failure: None,
    }));
    run_producers.borrow_mut().push(Rc::downgrade(&state));
    if run != NodeRun::ReadPerConsumer {
        states.insert(key, Rc::clone(&state));
    }
    Ok(state)
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
    let mut reached = Reached::new();
    for &root in roots {
        reach(dag, root, &context, &mut reached)?;
    }
    for (&(id, time), (count, _)) in &reached {
        let node = &dag.nodes[&id];
        if let (Some(time), NodeRun::Streamed, 2..) = (time, run_as(node, &context), *count) {
            return Err(Error::Invalid(format!(
                "node {id} ({}) at evaluation time {time} has {count} consumers but \
                 Lifetime::None; a shared query-time output must be OneEvaluation (planner bug)",
                node.operator.name()
            )));
        }
    }
    let mut states = BTreeMap::new();
    let run_producers = RunProducers::default();
    roots
        .iter()
        .map(|&id| {
            build(dag, id, &context, &reached, &mut states, &run_producers).map(Input::subscribe)
        })
        .collect()
}
/// Every producer of one run, so a cancellation seen by one releases the
/// batches the others hold.
type RunProducers<'a, V> = Rc<RefCell<Vec<Weak<RefCell<Producer<'a, V>>>>>>;

/// One run of a node, read by every consumer it has in the run.
struct Producer<'a, V> {
    node: NodeId,
    operation: String,
    stream: Option<OutputStream<'a, V>>,
    size: Box<dyn Fn(&V) -> usize + 'a>,
    context: RunContext,
    /// At query time, a materialized node ([`NodeRun::Held`]) is computed
    /// to the end before any consumer reads it, and its batches stay
    /// charged to the run until every consumer has read them.
    hold_until_finished: bool,
    /// How many consumers subscribe to it in the run; its stream is dropped
    /// early only once all have subscribed and all have left.
    expected_readers: usize,
    /// The run's producers, this one included.
    run_producers: RunProducers<'a, V>,
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
    /// The end of the node's output. A failed held output is not read in
    /// part: every consumer gets only the error.
    fn finish(&mut self, failure: Option<Error>) {
        if failure.is_some() && self.hold_until_finished {
            self.unread.values_mut().for_each(VecDeque::clear);
        }
        self.failure = failure;
        self.finished = true;
        self.stream = None;
        self.wake_all();
    }
    /// Stop the run: every consumer gets `Cancelled` next, and the batches
    /// not yet read are released.
    fn cancel(&mut self) {
        if self.failure != Some(Error::Cancelled) {
            self.finish(Some(Error::Cancelled));
            self.unread.values_mut().for_each(VecDeque::clear);
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
        // A consumer may drop its input in `start`, before a later consumer
        // subscribes; the stream is kept for that one.
        if state.unread.is_empty() && state.next_reader as usize >= state.expected_readers {
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
            if state.context.is_cancelled() && state.failure != Some(Error::Cancelled) {
                state.cancel();
                // The others; a producer being polled up the stack sees the
                // cancellation itself.
                let run_producers = Rc::clone(&state.run_producers);
                for producer in run_producers.borrow().iter().filter_map(Weak::upgrade) {
                    if let Ok(mut producer) = producer.try_borrow_mut() {
                        producer.cancel();
                    }
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
