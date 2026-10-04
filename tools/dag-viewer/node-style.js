// Operator kinds (`Operator::kind_name`) exported by
// crates/types/src/dag_export.rs. Categories describe the visible logical DAG
// shape. They do not model hidden physical inputs: for example,
// PromqlInfoEnrich is a one-child enrichment here even if physical costing
// later accounts for an auxiliary source scan.
const KIND_CATEGORY_JSON = `{
  "Scan": "data",
  "Values": "data",
  "Filter": "filter",
  "PromqlSeriesSample": "sample",
  "Project": "derive",
  "PromqlRelabel": "derive",
  "PromqlInfoEnrich": "derive",
  "PromqlVectorFromScalar": "derive",
  "BinaryOp": "derive",
  "Aggregate": "aggregate",
  "TimeRange": "window",
  "PromqlSubquery": "window",
  "TimeShift": "window",
  "SQLWindowFunc": "window",
  "Join": "join",
  "Dedup": "set",
  "SetOp": "set",
  "Concat": "combine",
  "Sort": "sort",
  "Limit": "sort",
  "SummaryAgg": "summary",
  "SummaryJoin": "summary",
  "SummarySubtract": "summary",
  "SummaryDelete": "summary",
  "SummaryEstimate": "summary",
  "SummaryMerge": "summary",
  "FinalizeExactAccumulator": "summary",
  "MaintainPopulation": "summary",
  "EvaluatePopulation": "summary",
  "Extension": "summary"
}`;
const KIND_CATEGORY = Object.freeze(JSON.parse(KIND_CATEGORY_JSON));

// The three node groups the viewer colors: data sources, summary operators
// (build, merge, estimate, finalize), and every other relational operator.
// An unknown kind is drawn as relational and logged, not guessed silently.
function nodeGroup(kind) {
  const category = KIND_CATEGORY[kind];
  if (category === undefined) {
    console.warn(`node-style.js: no KIND_CATEGORY entry for ${JSON.stringify(kind)}`);
    return 'rel';
  }
  if (category === 'data') return 'source';
  return category === 'summary' ? 'summary' : 'rel';
}
