//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-frontend-sql --test sql <file>::`.

mod frequency_entropy;
mod frequency_l2;
mod pearson_corr;
mod sql_lowering;
mod temporal_types;
