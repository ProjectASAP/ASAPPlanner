//! Stage 3 pricing: the [`CostModel`](cost_model::CostModel) trait every
//! deployment's cost-based selection plugs into, with the built-in
//! [`DefaultCostModel`](cost_model::DefaultCostModel); recurring and one-shot
//! cost rates ([`recurrence`]); analytical and evidence-based pricing
//! ([`analytical_cost`], [`empirical_cost`]); and
//! the physical lowering and storage I/O profiles they price
//! ([`query_physical_lowering`], [`storage_io`]).

pub mod analytical_cost;
pub mod cost_model;
pub mod empirical_cost;
pub mod empirical_resources;
pub mod physical_handoff_cost;
pub mod physical_operator_statistics;
pub mod query_physical_lowering;
pub mod recurrence;
pub mod storage_io;
