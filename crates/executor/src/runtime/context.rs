use crate::summary_kernels::sample_blocks::{PlainSampleBlockCodec, SampleBlockCodec};
use crate::Error;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    task::Waker,
};
/// Scope is part of an execution instance, never mutable state in a reusable plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Ingestion {
        window_start_ms: i64,
        window_end_ms: i64,
    },
    Query {
        evaluation_time_ms: i64,
    },
}
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
        }
    }
}
pub(super) struct Control {
    cancelled: Cell<bool>,
    bytes: Cell<usize>,
    peak: Cell<usize>,
    pub(super) limits: Limits,
    waiters: RefCell<Vec<Waker>>,
}
#[derive(Clone)]
pub struct RunContext {
    pub scope: Scope,
    query_range: Option<(i64, i64)>,
    /// The data's declared bound on how late a raw row may arrive after a
    /// later row of its series, in ms (`DataArrival`'s
    /// `max_out_of_order_ms`); `None` when undeclared.
    max_out_of_order_ms: Option<u64>,
    /// The codec compressed-samples states are built with: the
    /// deployment's, else the uncompressed reference.
    sample_block_codec: Arc<dyn SampleBlockCodec>,
    pub(super) control: Rc<Control>,
}
impl RunContext {
    pub fn new(scope: Scope, limits: Limits) -> Result<Self, Error> {
        if limits.max_bytes == 0 {
            return Err(Error::Invalid("execution limits must be positive".into()));
        }
        if matches!(&scope, Scope::Ingestion { window_start_ms, window_end_ms, .. } if window_start_ms > window_end_ms)
        {
            return Err(Error::Invalid("inverted ingestion window".into()));
        }
        Ok(Self {
            scope,
            query_range: None,
            max_out_of_order_ms: None,
            sample_block_codec: Arc::new(PlainSampleBlockCodec),
            control: Rc::new(Control {
                cancelled: Cell::new(false),
                bytes: Cell::new(0),
                peak: Cell::new(0),
                limits,
                waiters: RefCell::new(Vec::new()),
            }),
        })
    }
    /// Supply the outer range query bounds for PromQL `@ start()` / `@ end()`
    /// and for `@`-pinned calls, which are evaluated once at the query start.
    /// A range query run step by step must always supply them; without bounds
    /// each step is treated as its own instant query. For an instant query,
    /// supply the evaluation instant as both bounds.
    pub fn with_query_range(mut self, start_ms: i64, end_ms: i64) -> Result<Self, Error> {
        if start_ms > end_ms || !matches!(self.scope, Scope::Query { .. }) {
            return Err(Error::Invalid(
                "query range requires ordered bounds and query scope".into(),
            ));
        }
        self.query_range = Some((start_ms, end_ms));
        Ok(self)
    }
    pub(crate) fn query_range(&self) -> Option<(i64, i64)> {
        self.query_range
    }
    /// Declare that the raw rows this run reads arrive, in the order the
    /// data store returns them, at most `max_out_of_order_ms` behind a
    /// later row of their series: the data workload's
    /// `max_out_of_order_ms`. A counter build (rate, increase) then holds
    /// each row until the newest row of its series is more than the bound
    /// past it and folds the rows in time order: one series' rows at
    /// 10 s, 12 s, 11 s with a 5 s bound build the same state as rows at
    /// 10 s, 11 s, 12 s. Without it, a counter row not after its series'
    /// previous row is an operator error.
    pub fn with_max_out_of_order_ms(mut self, max_out_of_order_ms: u64) -> Self {
        self.max_out_of_order_ms = Some(max_out_of_order_ms);
        self
    }
    pub(crate) fn max_out_of_order_ms(&self) -> Option<u64> {
        self.max_out_of_order_ms
    }
    /// Build this run's compressed-samples states with `codec`, the
    /// deployment's sample-block encoding: each window's open block is
    /// `codec.new_block()`, and its sealed bytes are what the run's view
    /// rows hold and the deployment stores. Without it a run uses
    /// [`PlainSampleBlockCodec`], the uncompressed reference. A state read
    /// back keeps the codec that wrote it.
    pub fn with_sample_block_codec(mut self, codec: Arc<dyn SampleBlockCodec>) -> Self {
        self.sample_block_codec = codec;
        self
    }
    /// The codec this run builds compressed-samples states with
    /// ([`Self::with_sample_block_codec`]).
    pub fn sample_block_codec(&self) -> &Arc<dyn SampleBlockCodec> {
        &self.sample_block_codec
    }
    /// This run's context, evaluated at `evaluation_time_ms` instead: the
    /// same limits, cancellation, query range and `max_out_of_order_ms`.
    pub(crate) fn at_evaluation_time(&self, evaluation_time_ms: i64) -> Self {
        Self {
            scope: Scope::Query { evaluation_time_ms },
            ..self.clone()
        }
    }

    pub fn cancel(&self) {
        self.control.cancelled.set(true);
        for waiter in self.control.waiters.borrow_mut().drain(..) {
            waiter.wake();
        }
    }
    /// The query's evaluation time (SQL `now()`); `None` in an ingestion run.
    pub fn evaluation_time_ms(&self) -> Option<i64> {
        match self.scope {
            Scope::Query {
                evaluation_time_ms, ..
            } => Some(evaluation_time_ms),
            Scope::Ingestion { .. } => None,
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.control.cancelled.get()
    }
    /// Bytes reserved and not yet released: the Planner's own tests check
    /// that a finished run releases everything (`test-support`).
    #[cfg(any(test, feature = "test-support"))]
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
    pub(super) fn register(&self, waker: &Waker) {
        let mut waiters = self.control.waiters.borrow_mut();
        if !waiters.iter().any(|old| old.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }
}
pub struct Reservation {
    bytes: usize,
    pub(super) control: Rc<Control>,
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
