#!/usr/bin/env python3
"""Run every vendored parser test and reject any baseline drift."""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "crates/metricsql-parser-vendored/Cargo.toml"

EXPECTED_LIB_FAILURES = {
    "label::label_filter::tests::test_find_matchers",
    "optimizer::push_down_filters_test::tests::test_label_manipulation_functions",
    "optimizer::push_down_filters_test::tests::test_optimize_aggregate_funcs",
    "optimizer::push_down_filters_test::tests::test_optimize_transform_funcs",
    "optimizer::push_down_filters_test::tests::test_pushdown_binary_op_filters",
    "optimizer::simplifier::tests::test_simplify_div_by_one",
    "optimizer::simplifier::tests::test_simplify_selector_div_selector_same",
    "parser::expand_with_test::tests::test_expand_with_exprs_error",
    "parser::expand_with_test::tests::test_expand_with_exprs_success",
    "parser::parser_test::tests::complex_with_expressions",
    "parser::parser_test::tests::invalid_with_expr",
    "parser::parser_test::tests::nested_with_expressions",
    "parser::parser_test::tests::test_or_filters",
    "parser::parser_test::tests::test_parse_aggr_func_expr",
    "parser::parser_test::tests::test_parse_metric_expr_with_or",
    "parser::parser_test::tests::with_expr",
    "parser::parser_test::tests::with_expr_funcs",
    "parser::tokens::tests::misc_numbers",
    "parser::tokens::tests::strings",
    "parser::tokens::tests::various_durations",
    "parser::utils::tests::test_unescape_ident",
}

EXPECTED_DOC_FAILURES = {
    "src/ast/expr.rs - ast::expr::Expr::series_names",
    "src/ast/expr.rs - ast::expr::MetricExpr",
    "src/optimizer/simplifier.rs - optimizer::simplifier::ExprSimplifier::simplify",
}


def run(kind: str) -> str:
    command = ["cargo", "test", "--manifest-path", str(MANIFEST)]
    if kind == "lib":
        command += ["--lib", "--", "--test-threads=1"]
    else:
        command += ["--doc"]
    env = os.environ.copy()
    env["CARGO_TERM_COLOR"] = "never"
    env.setdefault("CARGO_TARGET_DIR", str(ROOT / "target/metricsql-vendored"))
    completed = subprocess.run(command, cwd=ROOT, env=env, text=True, capture_output=True)
    output = completed.stdout + completed.stderr
    if completed.returncode == 0:
        raise RuntimeError(f"{kind} baseline unexpectedly has no failures")
    return output


def failure_names(output: str) -> set[str]:
    matches = re.findall(r"\nfailures:\n((?:    [^\n]+\n)+)\ntest result:", output)
    if not matches:
        raise RuntimeError("cargo output did not contain a failure summary")
    return {
        re.sub(r" \(line [0-9]+\)$", "", line.strip())
        for line in matches[-1].splitlines()
    }


def verify(kind: str, expected: set[str], result_pattern: str) -> None:
    output = run(kind)
    actual = failure_names(output)
    if actual != expected or result_pattern not in output:
        print(output, file=sys.stderr)
        missing = sorted(expected - actual)
        added = sorted(actual - expected)
        raise RuntimeError(
            f"{kind} baseline drifted; missing={missing}, unexpected={added}"
        )
    print(f"vendored MetricsQL {kind}: verified {len(expected)} documented upstream failures")


verify("lib", EXPECTED_LIB_FAILURES, "232 passed; 21 failed")
verify("doc", EXPECTED_DOC_FAILURES, "2 passed; 3 failed")
