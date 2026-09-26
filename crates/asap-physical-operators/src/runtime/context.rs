use crate::Error;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    task::Waker,
};
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
    pub(super) control: Rc<Control>,
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
