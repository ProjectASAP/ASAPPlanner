//! Stage 3 pricing: analytical operator costs ([`analytical_cost`]) over
//! edge statistics ([`physical_operator_statistics`]), evaluation rates from
//! query recurrence ([`recurrence`]), and the physical lowering and storage
//! I/O profiles a deployment can price ([`query_physical_lowering`],
//! [`storage_io`], [`physical_handoff_cost`]).

pub mod analytical_cost;
pub mod physical_handoff_cost;
pub mod physical_operator_statistics;
pub mod query_physical_lowering;
pub mod recurrence;
pub mod storage_io;
