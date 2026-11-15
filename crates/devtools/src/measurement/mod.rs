//! Running selected plans on the Planner's executor and measuring them,
//! behind the `asap_run` binary:
//!
//! - [`execution`]: the reference driver. Plan a workload, install the plan
//!   (its materialized views and ingestion refresh groups), ingest each
//!   refresh window, evaluate at given times and read every root's answers
//!   (`asap_run execute`).
//! - [`samples`]: the data a run ingests, read from a JSONL file or
//!   generated from the workload's source facts with `aqpbm-datagen`.
//! - [`cost_calibration`] and [`accuracy_calibration`]: microbenchmarks of
//!   every operator, phase and state family, fitted into the planner's
//!   `MeasuredOperationCosts`, and seeded trials of every sketch
//!   configuration, recorded as `MeasuredSketchAccuracy` (`asap_run
//!   calibrate`).
//! - [`machine`] and [`calibration_tables`]: the machine key a calibration
//!   is stored under, and which tables a planning run reads.
//! - [`benchmark`]: each arm (exact, planned, no raw data) end to end, the
//!   planner's predicted cost beside the measured one, and the gate
//!   (`asap_run bench`).
//! - [`source_facts`]: `SourceFacts` computed from data, like a database's
//!   `ANALYZE` (`asap_run facts`).
//!
//! The planner library only reads `CostInputs`, an `AccuracyEvidence` and
//! `SourceFacts`; every path, machine and data adapter lives here.

pub mod accuracy_calibration;
pub mod benchmark;
pub mod calibration_tables;
pub mod cost_calibration;
pub mod execution;
pub mod machine;
pub mod samples;
pub mod source_facts;
