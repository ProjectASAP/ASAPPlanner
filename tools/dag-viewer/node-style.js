// Logical QueryExpr/SummaryExpr kinds exported by
// crates/types/src/dag_export.rs. Categories describe the visible logical DAG
// shape. They do not model hidden physical inputs: for example,
// PromqlInfoEnrich is a one-child enrichment here even if physical costing
// later accounts for an auxiliary source scan.
const KIND_CATEGORY_JSON = `{
  "Scan": "data",
  "PromqlScalarBridge": "data",
  "EvalTimestamp": "data",
  "CurrentTimestamp": "data",
  "Filter": "filter",
  "PromqlSeriesSample": "sample",
  "Project": "derive",
  "PromqlRelabel": "derive",
  "PromqlInfoEnrich": "derive",
  "PromqlVectorFromScalar": "derive",
  "PromqlScalarFromVector": "derive",
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
  "KeepPreAsap": "summary",
  "SummaryAgg": "summary",
  "SummaryJoin": "summary",
  "SummarySubtract": "summary",
  "SummaryBinaryOp": "summary",
  "SummaryDelete": "summary",
  "SummaryEstimate": "summary",
  "SummaryMerge": "summary"
}`;
const KIND_CATEGORY = Object.freeze(JSON.parse(KIND_CATEGORY_JSON));

// name -> { label, description, light: {bg, border}, dark: {bg, border} }
const CATEGORIES = {
  data: {
    label: 'Data',
    description: 'Scan, PromqlScalarBridge, EvalTimestamp, CurrentTimestamp — leaves that introduce a value',
    light: { bg: '#eef5fd', border: '#0369a1' },
    dark: { bg: '#0c2438', border: '#38bdf8' },
  },
  filter: {
    label: 'Filter',
    description: 'Filter — narrows rows by a boolean predicate',
    light: { bg: '#edf8ec', border: '#0f766e' },
    dark: { bg: '#072a20', border: '#2dd4bf' },
  },
  sample: {
    label: 'Sample',
    description: 'PromqlSeriesSample — keeps a deterministic subset of series by quota, not by predicate',
    light: { bg: '#fff5ea', border: '#b45309' },
    dark: { bg: '#3a2408', border: '#fb923c' },
  },
  derive: {
    label: 'Derive',
    description: 'Project, PromqlRelabel, PromqlInfoEnrich, PromqlVectorFromScalar, PromqlScalarFromVector, BinaryOp — transforms or enriches columns on otherwise-unchanged rows',
    light: { bg: '#f5f0fd', border: '#6d28d9' },
    dark: { bg: '#241a3d', border: '#a78bfa' },
  },
  aggregate: {
    label: 'Aggregate',
    description: 'Aggregate — groups and reduces',
    light: { bg: '#fffbe8', border: '#a16207' },
    dark: { bg: '#332b05', border: '#facc15' },
  },
  window: {
    label: 'Window',
    description: 'TimeRange, PromqlSubquery, TimeShift, SQLWindowFunc — reads or positions a scoped window of rows/time around each row',
    light: { bg: '#fdf1f6', border: '#be185d' },
    dark: { bg: '#3a1626', border: '#f472b6' },
  },
  join: {
    label: 'Join',
    description: 'Join — combines two DAG inputs on a predicate',
    light: { bg: '#edf9f8', border: '#15803d' },
    dark: { bg: '#08302c', border: '#4ade80' },
  },
  set: {
    label: 'Set',
    description: 'Dedup, SetOp — enforces or computes set semantics (dedup rows, or union/intersect/except of two relations)',
    light: { bg: '#f1efff', border: '#4f46e5' },
    dark: { bg: '#221f3d', border: '#a5b4fc' },
  },
  combine: {
    label: 'Combine',
    description: 'Concat — concatenates n branches with no dedup (UNION ALL-shaped fan-in)',
    light: { bg: '#f7fee7', border: '#4d7c0f' },
    dark: { bg: '#1a2e05', border: '#a3e635' },
  },
  sort: {
    label: 'Sort',
    description: 'Sort, Limit — orders or caps rows',
    light: { bg: '#eef4fd', border: '#1d4ed8' },
    dark: { bg: '#12233d', border: '#60a5fa' },
  },
  // Post-ASAP nodes use a neutral palette; KeepPreAsap has a muted override.
  summary: {
    label: 'Summary',
    description: 'KeepPreAsap, SummaryBinaryOp, SummaryAgg, SummaryJoin, SummarySubtract, SummaryDelete, SummaryEstimate, SummaryMerge — post-ASAP materialized structures',
    light: { bg: '#f1f2f4', border: '#4b5563' },
    dark: { bg: '#20242b', border: '#9ca3af' },
  },
  // Loud fallback for malformed or version-skewed exports.
  unknown: {
    label: 'Unknown kind',
    description: 'A DagNode.kind with no KIND_CATEGORY entry — update node-style.js',
    light: { bg: '#fef2f2', border: '#b91c1c' },
    dark: { bg: '#2a1212', border: '#f87171' },
  },
};

for (const [kind, category] of Object.entries(KIND_CATEGORY)) {
  if (!Object.prototype.hasOwnProperty.call(CATEGORIES, category)) {
    throw new Error(`DagNode kind ${kind} uses undeclared category ${category}`);
  }
}

const ROOT_BADGE = {
  label: 'Query root',
  description: "This node is the query's output",
  light: { border: '#b42318' },
  dark: { border: '#f87171' },
};

function isDarkMode() {
  return window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
}

function categoryOf(kind) {
  const category = KIND_CATEGORY[kind];
  if (category) return category;
  console.warn(
    `node-style.js: DagNode.kind ${JSON.stringify(kind)} has no KIND_CATEGORY entry — ` +
    'rendering as "Unknown kind" instead of silently guessing. Add an entry to KIND_CATEGORY.'
  );
  return 'unknown';
}

function categoryColors(name) {
  const cat = CATEGORIES[name] || CATEGORIES.unknown;
  return isDarkMode() ? cat.dark : cat.light;
}

function applyPaletteToCssVars() {
  const root = document.documentElement.style;
  for (const [name, category] of Object.entries(CATEGORIES)) {
    const colors = isDarkMode() ? category.dark : category.light;
    root.setProperty(`--cat-${name}-bg`, colors.bg);
    root.setProperty(`--cat-${name}-border`, colors.border);
  }
  root.setProperty('--cat-output-border', (isDarkMode() ? ROOT_BADGE.dark : ROOT_BADGE.light).border);
}

applyPaletteToCssVars();
if (window.matchMedia) {
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', applyPaletteToCssVars);
}
