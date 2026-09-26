//! Per-run producer sharing, streams, backpressure and resource ownership.
use crate::{
    plan::{NodeId, PhysicalDag},
    Error,
};
use futures::{stream::LocalBoxStream, Stream};
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

pub(crate) fn execute<'r, V: 'r, S: Clone + PartialEq + Debug + 'r>(
    dag: &'r PhysicalDag<'_, V, S>,
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
        .map(|&id| build(dag, id, &context, &mut states).map(Input::subscribe))
        .collect()
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

pub mod batch_execution;
#[cfg(test)]
mod tests;

mod cooperative;
pub(crate) use cooperative::Cooperative;
