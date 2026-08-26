// Category table for QueryExpr node kinds — the single source of truth for
// how the viewer colors and labels each `DagNode.kind`. Edit this file to
// reclassify a kind or retune a palette; nothing else in index.html needs to
// change.
//
// Also covers the 7 post-ASAP-only SummaryDagNode kinds (KeepPreAsap,
// SummaryAgg, SummaryJoin, SummarySubtract, SummaryDelete, SummaryEstimate,
// SummaryMerge) that appear in the post-ASAP lane — see the `summary`
// category below.

// kind (DagNode.kind from crates/ir/src/dag_export.rs) -> category name.
const KIND_CATEGORY = {
  Scan: 'data',
  Ref: 'data',
  Scalar: 'data',
  EvalTime: 'data',
  Filter: 'filter',
  Sample: 'filter',
  Project: 'derive',
  Relabel: 'derive',
  VectorFromScalar: 'derive',
  ScalarFromVector: 'derive',
  TimeShift: 'derive',
  BinaryOp: 'derive',
  InfoJoin: 'join',
  Join: 'join',
  Aggregate: 'aggregate',
  Window: 'window',
  WindowFunc: 'window',
  Subquery: 'window',
  TimeRange: 'window',
  Distinct: 'set',
  Merge: 'set',
  SetOp: 'set',
  Sort: 'sort',
  Limit: 'sort',
  LetBinding: 'bind',
  // Post-ASAP SummaryDagNode kinds (post-ASAP lane only).
  KeepPreAsap: 'summary',
  SummaryAgg: 'summary',
  SummaryJoin: 'summary',
  SummarySubtract: 'summary',
  SummaryDelete: 'summary',
  SummaryEstimate: 'summary',
  SummaryMerge: 'summary',
};

// name -> { label, description, light: {bg, border}, dark: {bg, border} }
const CATEGORIES = {
  data: {
    label: 'Data',
    description: 'Scan, Ref, Scalar, EvalTime — leaves that introduce a value',
    light: { bg: '#eef5fd', border: '#0369a1' },
    dark: { bg: '#0c2438', border: '#38bdf8' },
  },
  filter: {
    label: 'Filter',
    description: 'Filter, Sample — narrows rows',
    light: { bg: '#edf8ec', border: '#0f766e' },
    dark: { bg: '#072a20', border: '#2dd4bf' },
  },
  derive: {
    label: 'Derive',
    description: 'Project, Relabel, VectorFromScalar, ScalarFromVector, TimeShift, BinaryOp — transforms values',
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
    description: 'Window, WindowFunc, Subquery, TimeRange — scopes over a time range',
    light: { bg: '#fdf1f6', border: '#be185d' },
    dark: { bg: '#3a1626', border: '#f472b6' },
  },
  join: {
    label: 'Join',
    description: 'Join, InfoJoin — combines two inputs',
    light: { bg: '#edf9f8', border: '#15803d' },
    dark: { bg: '#08302c', border: '#4ade80' },
  },
  set: {
    label: 'Set',
    description: 'Distinct, Merge, SetOp — dedup or combine branches',
    light: { bg: '#f1efff', border: '#4f46e5' },
    dark: { bg: '#221f3d', border: '#a5b4fc' },
  },
  sort: {
    label: 'Sort',
    description: 'Sort, Limit — orders or caps rows',
    light: { bg: '#eef4fd', border: '#1d4ed8' },
    dark: { bg: '#12233d', border: '#60a5fa' },
  },
  bind: {
    label: 'Bind',
    description: 'LetBinding — names a sub-expression for reuse via Ref',
    light: { bg: '#fff5ea', border: '#b45309' },
    dark: { bg: '#3a2408', border: '#fb923c' },
  },
  // Post-ASAP only (post-ASAP lane): every other category
  // above is a saturated, hand-picked hue for a pre-ASAP QueryExpr operator.
  // `summary` is deliberately plain neutral gray instead of another hue —
  // partly because a 10th saturated color starts getting hard to
  // distinguish at a glance from its 9 neighbors (data's blue and sort's
  // blue are already close), and partly because "materialized post-ASAP
  // structure" reads better as a visually distinct *family* (muted,
  // grayscale) than as one more member of the pre-ASAP rainbow. KeepPreAsap
  // (unchanged pre-ASAP content passed through) gets its own even-more-muted
  // override in viewer.js's buildCyStyle rather than its own category, since
  // it's a variant *within* "post-ASAP" (something bothered to carry it
  // through) rather than a different concept.
  summary: {
    label: 'Summary',
    description: 'KeepPreAsap, SummaryAgg, SummaryJoin, SummarySubtract, SummaryDelete, SummaryEstimate, SummaryMerge — post-ASAP materialized structures',
    light: { bg: '#f1f2f4', border: '#4b5563' },
    dark: { bg: '#20242b', border: '#9ca3af' },
  },
};

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
  return KIND_CATEGORY[kind] || 'derive';
}

function categoryColors(name) {
  const cat = CATEGORIES[name] || CATEGORIES.derive;
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
