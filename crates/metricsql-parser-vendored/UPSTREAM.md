# Upstream provenance

This crate vendors the `parser` directory from
[`ccollie/metricsql`](https://github.com/ccollie/metricsql) commit
`3046709308e449a42c56bfbfd45f95af848e6768` (Apache-2.0).

`Cargo.toml` expands the upstream workspace dependencies and points
`metricsql_common` to ASAPPlanner's stable parser-only support crate. The Rust
sources differ from upstream only by `cargo fmt`; token-level source parity was
checked when vendoring. Update the pinned commit and repeat that comparison
before changing the vendored parser.
