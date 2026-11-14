# Extending the planner

*This chapter is an outline; its content follows.*

This chapter walks through the two most common contributions: a new sketch
family and a new reuse rule. It is for contributors, and complements the
recipes in [extend ASAP-aware mapping](https://github.com/ProjectASAP/ASAPPlanner/blob/main/docs/develop_docs/extend-asap-aware-mapping.md).

- Adding a sketch family, one stage at a time:
  - its type and parameters (`SketchAlgorithm`, `SketchParams`,
    `parameter_list`) and its capability row
    (`SketchAlgorithm::capability`, read by `SummaryTypeCapability`);
  - offering it in Pass 1 (`ALGORITHM_PREFERENCE`, `summary_candidates`);
  - its error model (`theoretical_guarantee`) and its cost
    (`sketch_shape`);
  - its executor kernel (`summary_kernels`, `EVALUATED_READOUTS`).
- Adding a reuse rule: a memo rule under Pass 2's `rules` that adds groups
  (`Memo::find_or_add_group`) or candidates (`Memo::add_candidate`), called
  from `generate_logical_asap_memo_candidates`.
- The rules every extension keeps: no stage calls another, a candidate is
  pruned only with a recorded reason, never silently dropped (Pass 2's
  dead-candidate elimination, Stage 3's `Selection::pruned_candidates`), and
  summary families are read only through
  `SummaryTypeCapability`.
- Testing an extension: unit tests beside the change and an end-to-end test
  in `integration-tests` that the planned workload uses it.
