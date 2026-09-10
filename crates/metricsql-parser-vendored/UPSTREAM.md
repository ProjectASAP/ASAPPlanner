# Upstream provenance

This crate vendors the `parser` directory from
[`ccollie/metricsql`](https://github.com/ccollie/metricsql) commit
`3046709308e449a42c56bfbfd45f95af848e6768` (Apache-2.0).

`Cargo.toml` expands the upstream workspace dependencies and points
`metricsql_common` to ASAPPlanner's stable parser-only support crate. Parser
changes beyond `cargo fmt` are limited to compatibility fixes covered by the
official VictoriaMetrics Go parser golden corpus in
`crates/frontend-metricsql/tests/victoriametrics_go_golden.rs`.

At the pinned commit, the upstream crate does not compile without changing its
`HashSetExt` import from `metricsql_common` to `ahash`. With that compile-only
fix and upstream's nightly toolchain, its library baseline is 230 passed and 23
failed; its doctest baseline is 2 passed and 3 failed. The failures cover stale
optimizer/parser expectations and lexer edge cases. Our numeric underscore fix
resolves two of those lexer failures. The remaining 21 library and 3 doctest
failures are named explicitly and run on every CI build by
`tools/verify_metricsql_vendored_baseline.py`; any added or removed failure
causes CI to fail. Update the pinned commit, reproduce the upstream baseline,
and update that list before changing the vendored parser again.
