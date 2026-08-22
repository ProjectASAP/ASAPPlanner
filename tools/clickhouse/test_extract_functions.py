"""Unit tests for `extract_functions.py`'s catalog-parsing and diff logic --
deliberately exercised without a live (or embedded) ClickHouse: `chdb` is
never imported here, matching `extract_functions.fetch_clickhouse_aggregate_names`
being the only function in that module that needs it.

Run with: python3 -m unittest tools/clickhouse/test_extract_functions.py
(or `python3 -m pytest tools/clickhouse/` if pytest is available -- these
are plain `unittest.TestCase`s, either runner works). Not wired into CI --
see tools/clickhouse/README.md for why.
"""

from __future__ import annotations

import unittest

from extract_functions import combinator_base, diff, format_report, parse_catalog_names

# A trimmed stand-in for crates/sql-function-catalog/src/lib.rs's shape --
# enough surrounding structure (a preceding NATIVE_FUNCTIONS-like table, the
# doc comment, the real field layout) to prove the parser targets the right
# block and ignores everything else, without depending on the real file's
# exact current contents (which is free to keep growing).
FIXTURE_SOURCE = '''
pub const NATIVE_FUNCTIONS: &[NativeFunction] = &[
    NativeFunction {
        name: "sum",
        arity: Arity::Exact(1),
        semantic: AggSemantic::Sum,
    },
];

/// ClickHouse-only builtin aggregate names DataFusion's planner has no
/// native equivalent for at all.
pub const CLICKHOUSE_BUILTINS: &[ClickHouseBuiltin] = &[
    ClickHouseBuiltin {
        name: "uniqexact",
        arity: Arity::Exact(1),
        rewrite: RewriteKind::CountDistinct,
    },
    ClickHouseBuiltin {
        name: "countif",
        arity: Arity::Exact(1),
        rewrite: RewriteKind::CountIfToSum,
    },
];

pub fn lookup_native(name: &str) -> Option<AggSemantic> {
    NATIVE_FUNCTIONS.iter().find(|f| f.name == name).map(|f| f.semantic)
}
'''


class ParseCatalogNamesTest(unittest.TestCase):
    def test_extracts_every_name_in_the_clickhouse_builtins_block(self):
        self.assertEqual(parse_catalog_names(FIXTURE_SOURCE), {"uniqexact", "countif"})

    def test_does_not_pick_up_native_functions_entries(self):
        names = parse_catalog_names(FIXTURE_SOURCE)
        self.assertNotIn("sum", names)

    def test_raises_a_clear_error_when_the_const_is_missing(self):
        with self.assertRaisesRegex(ValueError, "CLICKHOUSE_BUILTINS"):
            parse_catalog_names("pub const NATIVE_FUNCTIONS: &[NativeFunction] = &[];")

    def test_raises_a_clear_error_when_the_closing_bracket_is_missing(self):
        truncated = FIXTURE_SOURCE.split("pub const CLICKHOUSE_BUILTINS")[0] + (
            'pub const CLICKHOUSE_BUILTINS: &[ClickHouseBuiltin] = &[\n'
            '    ClickHouseBuiltin { name: "uniqexact", '
        )
        with self.assertRaisesRegex(ValueError, "closing"):
            parse_catalog_names(truncated)


class CombinatorBaseTest(unittest.TestCase):
    def test_recognizes_the_if_combinator(self):
        self.assertEqual(combinator_base("countif"), "count")

    def test_recognizes_the_distinct_combinator(self):
        self.assertEqual(combinator_base("sumdistinct"), "sum")

    def test_prefers_the_longer_suffix_over_a_shorter_one_it_contains(self):
        # "ordefault" must not be misparsed as bare "if"/"or" matches.
        self.assertEqual(combinator_base("sumordefault"), "sum")

    def test_returns_none_for_a_name_with_no_known_combinator_suffix(self):
        self.assertIsNone(combinator_base("uniqexact"))

    def test_returns_none_rather_than_an_empty_base(self):
        # A name that IS a bare suffix (no base in front) isn't a combinator
        # of anything.
        self.assertIsNone(combinator_base("if"))


class DiffTest(unittest.TestCase):
    def test_candidates_are_clickhouse_names_the_catalog_lacks(self):
        candidates, stale, combinator_derived = diff(
            catalog_names={"uniqexact"},
            clickhouse_names={"uniqexact", "quantile", "median"},
        )
        self.assertEqual(candidates, ["median", "quantile"])
        self.assertEqual(stale, [])
        self.assertEqual(combinator_derived, [])

    def test_a_removed_upstream_name_is_reported_stale(self):
        candidates, stale, combinator_derived = diff(
            catalog_names={"uniqexact", "somethingremoved"},
            clickhouse_names={"uniqexact"},
        )
        self.assertEqual(stale, ["somethingremoved"])
        self.assertEqual(combinator_derived, [])

    def test_a_combinator_of_a_still_present_base_is_not_reported_stale(self):
        # "countif" isn't its own system.functions row -- it's "count" + the
        # -If combinator -- so it must land in combinator_derived, not stale.
        # "count" itself is a legitimate candidate here (the catalog only
        # lists "countif", not the bare base) -- this test is only about
        # `stale`/`combinator_derived`, so it isn't asserted on.
        candidates, stale, combinator_derived = diff(
            catalog_names={"countif"},
            clickhouse_names={"count"},
        )
        self.assertEqual(stale, [])
        self.assertEqual(combinator_derived, [("countif", "count")])
        self.assertEqual(candidates, ["count"])

    def test_a_combinator_whose_base_is_also_gone_is_reported_stale_not_combinator_derived(self):
        # If the base itself vanished too, that's a real gap worth flagging,
        # not quietly absorbed into "combinator, nothing to see here".
        candidates, stale, combinator_derived = diff(
            catalog_names={"countif"},
            clickhouse_names=set(),
        )
        self.assertEqual(stale, ["countif"])
        self.assertEqual(combinator_derived, [])

    def test_empty_inputs_produce_empty_everything(self):
        self.assertEqual(diff(set(), set()), ([], [], []))


class FormatReportTest(unittest.TestCase):
    def test_report_is_sorted_and_diff_friendly(self):
        report = format_report(["a", "b"], ["z"], [("countif", "count")])
        lines = report.splitlines()
        # Headers present in a stable order; a byte-for-byte snapshot would
        # be too brittle across incidental wording tweaks, so this only
        # pins the properties the tool's usefulness actually depends on.
        self.assertIn("Candidates", report)
        self.assertIn("Possibly stale", report)
        self.assertIn("Combinator-derived", report)
        self.assertLess(lines.index("  a"), lines.index("  b"))
        self.assertIn("  z", report)
        self.assertIn("countif", report)

    def test_empty_sections_say_so_rather_than_being_blank(self):
        report = format_report([], [], [])
        self.assertIn("(none)", report)


if __name__ == "__main__":
    unittest.main()
