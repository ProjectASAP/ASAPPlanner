//! The `asap_run` test suite (`asap_devtools::measurement`), by the test
//! plan's sections. Tiers: T0 and T1 run in CI; T2 (timing) is ignored
//! with "timing: run on a dedicated host"; T3 (milestone scale) with
//! "milestone scale". Every expected value is computed by hand, from a
//! kernel or clock of known cost, from known sketch bounds or by
//! independent enumeration, never by the planner's own optimization or
//! cost code. A test that depends on a provisional answer of the test plan
//! says `// provisional answer N`.

mod common;
mod cost_accounting;
mod cost_fit;
mod end_to_end;
mod execution;
mod gate;
mod machine_and_tables;
mod measured_accuracy;
mod oracle;
mod regression;
mod source_facts;
