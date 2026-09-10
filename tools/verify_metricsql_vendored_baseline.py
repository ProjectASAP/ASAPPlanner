#!/usr/bin/env python3
"""Run every vendored parser test and reject any baseline drift."""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "crates/metricsql-parser-vendored/Cargo.toml"

EXPECTED_LIB_FAILURES = {
    'label::label_filter::tests::test_find_matchers': '7e37e854b833a77bab92ba276c493cc006341b0e9af3efca54c9335b9861676a',
    'optimizer::push_down_filters_test::tests::test_label_manipulation_functions': '4468095f0d9c97ba005d08a78b5d1d09b0919bccf77dd8830fd6df306d7e3403',
    'optimizer::push_down_filters_test::tests::test_optimize_aggregate_funcs': '5c1b9ced1dd6b841f7222678e96482921d424dae083dbef283b9ff2cf225943b',
    'optimizer::push_down_filters_test::tests::test_optimize_transform_funcs': 'a35cda3bcbe0a2382951c432cf9e1e285fbfbb55c84ca13c221a590480873427',
    'optimizer::push_down_filters_test::tests::test_pushdown_binary_op_filters': 'a339277e630399318773ebb277c01e0174bfd5bdc4b172d33fc1c50e39eb0021',
    'optimizer::simplifier::tests::test_simplify_div_by_one': '2c91cb4d92786af3a5432149f0775c25a3a6bec3ebd4165b04fdaa77e3fd1939',
    'optimizer::simplifier::tests::test_simplify_selector_div_selector_same': '0ae1afa9b70e4ee2b7901a6575460e927cb6cb27ce34997ea205c419358af0ae',
    'parser::expand_with_test::tests::test_expand_with_exprs_error': 'dbcf9477bbade0703c92676ce85ae80d8b841054e99a98c4e9e1788ea1dc3495',
    'parser::expand_with_test::tests::test_expand_with_exprs_success': 'aae10ca3c9f746c26cb9d0ca0299821eb15fe94833ae45f4b20c7c5140e94f44',
    'parser::parser_test::tests::complex_with_expressions': 'f928f457bd7b06919243559a5f09c45202daef66f4289ae2c27a25fd6e7c386b',
    'parser::parser_test::tests::invalid_with_expr': '052767ae3bcfb4c974356bde5022c608bbcd535713babce11ac788267b31fa68',
    'parser::parser_test::tests::nested_with_expressions': 'f91f6d2bab2a5b741156c748ccbe718f52d799216d55269c7d28f2ddcc21a83e',
    'parser::parser_test::tests::test_or_filters': 'ee61980e086cffb259fe5cb781d4fa3070e695113b3a08f9a995b083280c4cb4',
    'parser::parser_test::tests::test_parse_aggr_func_expr': '0dedea16a907ccdac6f95bd8467a1d41e0c5178013c0cd086fd3b1801e6a99e7',
    'parser::parser_test::tests::test_parse_metric_expr_with_or': '90ebb5fa0806b2e61c58afbd31bd4cd357afa36ac1fce6f233d2f0776377ccc5',
    'parser::parser_test::tests::with_expr': 'e77b8f131a5c34388381f9ad4ff6cee5a1a79520fadf01eabb6e275a713fc155',
    'parser::parser_test::tests::with_expr_funcs': '7257a6bfadd5c8e25dd9b7ffa62f2a7d463c51eac5158b969b7e8580ca66fe81',
    'parser::tokens::tests::misc_numbers': '64abdb0c659bb90fa612f7bcf8fdfc413b24eff2384cb306cf875ae1c7e89ace',
    'parser::tokens::tests::strings': '9db7352f09155dd1776f702b5f80a541d081ec32b6dffb70090589d862525f23',
    'parser::tokens::tests::various_durations': '3e10336b77e76f861f1de60a7e8037ffdf59e85b03683cbd63f52fcb7d48e181',
    'parser::utils::tests::test_unescape_ident': '0bf2d635dcdb0221c33b055fb2fbbc3980343ad375d72e7360634c67ef204974',
}

EXPECTED_DOC_FAILURES = {
    'src/ast/expr.rs - ast::expr::Expr::series_names': 'a6353e9ca306eb6768c1e0453f9a18830fb9335f2675a30d5a1be0635f4d826e',
    'src/ast/expr.rs - ast::expr::MetricExpr': '9c330538f87885aae58319c4b527f83bd10a659f0fcb9f2318adb0610f852396',
    'src/optimizer/simplifier.rs - optimizer::simplifier::ExprSimplifier::simplify': 'b8f549620335e16b37121a9ccdcb21ba33f066779459f32ddd13bbdcee07fd10',
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


def failure_signatures(output: str) -> dict[str, str]:
    blocks = re.findall(r"^---- (.+?) stdout ----\n(.*?)(?=^---- |^failures:|\Z)", output, re.MULTILINE | re.DOTALL)
    if not blocks:
        raise RuntimeError("cargo output did not contain per-test failure blocks")
    signatures = {}
    for name, body in blocks:
        name = re.sub(r" \(line [0-9]+\)$", "", name)
        body = re.sub(r"(thread '[^']+') \(\d+\)", r"\1", body)
        body = re.sub(r"(/[^\s:]+)+/([^/\s:]+\.rs):\d+:\d+", r"\2:<line>", body)
        body = re.sub(r"(?<![A-Za-z])(?:src/)?([A-Za-z_]+\.rs):\d+:\d+", r"\1:<line>", body)
        body = re.sub(r"\bline \d+\b", "line <n>", body)
        body = "\n".join(line.rstrip() for line in body.strip().splitlines())
        signatures[name] = hashlib.sha256(body.encode()).hexdigest()
    return signatures


def verify(kind: str, expected: dict[str, str], result_pattern: str) -> None:
    output = run(kind)
    actual = failure_signatures(output)
    if actual != expected or result_pattern not in output:
        print(output, file=sys.stderr)
        missing = sorted(expected.keys() - actual.keys())
        added = sorted(actual.keys() - expected.keys())
        changed = sorted(name for name in expected.keys() & actual.keys() if expected[name] != actual[name])
        raise RuntimeError(
            f"{kind} baseline drifted; missing={missing}, unexpected={added}, changed_signatures={changed}"
        )
    print(f"vendored MetricsQL {kind}: verified {len(expected)} documented upstream failures")


verify("lib", EXPECTED_LIB_FAILURES, "232 passed; 21 failed")
verify("doc", EXPECTED_DOC_FAILURES, "2 passed; 3 failed")
