"""Unit checks for measurement normalization, independent of running the benchmark."""
import unittest

from run import cpu_per_op, export


class NormalizationTests(unittest.TestCase):
    def test_complete_comparison_rejects_missing_live_phase_measurements(self):
        """An old constructor-only probe cannot label consuming-wrapper CPU as disjoint."""
        row = {"schema_version": 5, "query_accuracy": {}, "sketch": "cms", "impl": "lib",
               "sketch_config": {}, "workload": {}}
        manifest = {"invocations": [{}]}
        for resources in [[], [dict(row, build_cpu_ns_samples=[1.0])]]:
            with self.subTest(resources=resources):
                with self.assertRaisesRegex(ValueError, "disjoint live-state"):
                    export([row], manifest, [[]], [], resources)

    def test_partial_report_files_are_rejected_before_pairing(self):
        """A truncated report file must fail rather than silently lose invocations."""
        for raw, invocations, operations in [([{}], [], []), ([], [{}], []), ([], [], [{}])]:
            with self.subTest(lengths=(len(raw), len(invocations), len(operations))):
                with self.assertRaisesRegex(ValueError, "misaligned invocation reports"):
                    export(raw, {"invocations": invocations}, operations, [])

    def test_cpu_normalizes_paired_work_not_wall_time(self):
        """Different wall durations for identical work must not change the denominator."""
        row = {"query_throughput_items_per_sec": {"samples": [2000, 1000]},
               "query_wall_time_ms": {"samples": [100, 200]},
               "query_cpu_time_ms": {"user_ms": {"samples": [20, 20]},
                                     "sys_ms": {"samples": [10, 10]}}}
        result = cpu_per_op(row, "query")
        self.assertEqual(result["value"], 150000)
        self.assertEqual(result["stddev"], 0)
        self.assertEqual(result["samples"], 2)

    def test_unmeasured_cpu_is_not_zero(self):
        """Missing CPU evidence stays absent instead of looking like free work."""
        self.assertIsNone(cpu_per_op({}, "query"))

    def test_misaligned_samples_rejected(self):
        """CPU and rates from different run populations cannot be paired."""
        row = {"insert_throughput_items_per_sec": {"samples": [2000, 1000]},
               "insert_wall_time_ms": {"samples": [100]},
               "insert_cpu_time_ms": {"user_ms": {"samples": [20, 20]},
                                      "sys_ms": {"samples": [10, 10]}}}
        with self.assertRaises(ValueError):
            cpu_per_op(row, "insert")


if __name__ == "__main__":
    unittest.main()
