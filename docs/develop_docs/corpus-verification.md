# Corpus and coverage verification

Audience: contributors checking frontend lowering and replacement coverage.
Run these commands from the repository root. Coverage reports measure exercised
IR and planning opportunities; they do not establish runtime accuracy or physical
feasibility. See [corpus sources](metrics-observability-corpora.md).

## Compare corpus lowering

To dump and compare every PromQL corpus, run:

```sh
cargo run -p asap-devtools --bin analyze_corpora -- --corpora --out-dir artifacts/promql_pre_asap
```

This writes one successful-lowering JSONL dump and one error JSONL dump per
corpus, plus per-query JSON files under `<corpus>/` and `<corpus>.errors/`,
`summary.json`, and the heuristic `anomalies.md` report. The anomaly report
compares exact duplicates, whitespace/case-normalized queries, coarse
structural shapes, and distinct expressions that produce identical IR.

The corresponding SQL corpus analysis is:

```sh
cargo run -p asap-devtools --bin analyze_corpora -- --sql-corpora --out-dir artifacts/sql_pre_asap
```

## Check IR variant coverage

Parse the query corpora in the repository and report which pre-ASAP IR variants are exercised:

```sh
cargo run -p asap-devtools --bin variant_coverage
```

## Check sketch-replacement coverage

Parse the same query corpora, lower them with an approximate `AccuracyTarget`, and report what fraction of each corpus's successfully-lowered queries got a genuine sketch alternative (`SketchApproximation`, e.g. KLL vs. DDSketch) and/or a cross-query common-subexpression-reuse candidate (`CommonSubexpressionReuse`):

```sh
cargo run -p asap-devtools --bin sketch_coverage -- --epsilon 0.01
```

`--epsilon` is optional (defaults to `0.01`). See the binary's own doc comment for exactly how "coverage" is defined and attributed back to each query.
