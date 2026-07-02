//! Cost model (stub).
//!
//! The cost traits the optimizer consults — and, in particular, the model that
//! credits a hoisted [`cse`](crate::cse) producer once instead of per consumer.
//!
//! TODO(#6): wire workload-level CSE into a cost model.
//! TODO(#33): detect which optimizations are applicable to a query workload.
