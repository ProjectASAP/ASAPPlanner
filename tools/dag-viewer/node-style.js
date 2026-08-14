// Category table for QueryExpr node kinds — the single source of truth for
// how the viewer colors/labels/icons each `DagNode.kind`. Edit this file to
// reclassify a kind or retune a palette; nothing else in index.html needs to
// change. Icon SVGs are adapted from ProjectASAP/bgp-query-dag-explorer's
// hand-drawn icon set (see tools/dag-viewer/README.md).

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
};

const ICON_STROKE = '2.2';
const ICON_FILL = 'rgba(255,255,255,.06)';

// Each icon fn takes a stroke/fill color and returns a standalone SVG string
// (viewBox 0 0 48 48, consistent stroke weight) for use as a node background
// image. Cylinder/funnel/transform/grid/venn/dashed-circle/sort-arrow are
// adapted directly from the reference repo's `iconFor()`; `bind` and the
// root-node badge are new, drawn in the same style.
const ICONS = {
  data: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><ellipse cx="24" cy="10" rx="15" ry="6" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}"/><path d="M9 10v24c0 3.4 6.7 6 15 6s15-2.6 15-6V10" fill="none" stroke="${c}" stroke-width="${ICON_STROKE}"/><path d="M9 22c0 3.4 6.7 6 15 6s15-2.6 15-6" fill="none" stroke="${c}" stroke-width="${ICON_STROKE}"/></svg>`,
  filter: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><path d="M8 10h32L28 24v12l-8 4V24z" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}" stroke-linejoin="round"/></svg>`,
  derive: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><circle cx="13" cy="14" r="4" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}"/><circle cx="35" cy="14" r="4" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}"/><circle cx="24" cy="34" r="4" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}"/><path d="M17 16l14 0M15 18l7 13M33 18l-7 13" stroke="${c}" stroke-width="${ICON_STROKE}" fill="none" stroke-linecap="round"/></svg>`,
  aggregate: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><text x="24" y="34" font-size="30" font-weight="900" text-anchor="middle" font-family="ui-sans-serif,system-ui,sans-serif" fill="${c}">Σ</text></svg>`,
  window: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><rect x="9" y="10" width="30" height="28" rx="4" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}"/><path d="M16 10v28M24 10v28M32 10v28M9 20h30M9 29h30" stroke="${c}" stroke-width="1.6" opacity=".8"/><path d="M16 7l16 34" stroke="${c}" stroke-width="${ICON_STROKE}" stroke-linecap="round"/></svg>`,
  join: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><circle cx="19" cy="24" r="13" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}"/><circle cx="29" cy="24" r="13" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}"/></svg>`,
  set: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><circle cx="24" cy="24" r="16" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}" stroke-dasharray="3 3"/><circle cx="18" cy="22" r="2" fill="${c}"/><circle cx="26" cy="18" r="2" fill="${c}"/><circle cx="29" cy="29" r="2" fill="${c}"/><circle cx="20" cy="31" r="2" fill="${c}"/></svg>`,
  sort: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><path d="M12 12h20M12 22h15M12 32h10" stroke="${c}" stroke-width="${ICON_STROKE}" stroke-linecap="round"/><path d="M35 12v24m0 0l-6-6m6 6l6-6" stroke="${c}" stroke-width="${ICON_STROKE}" stroke-linecap="round" stroke-linejoin="round"/></svg>`,
  // New: a name tag, for LetBinding naming a sub-expression for reuse.
  bind: (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><path d="M8 22V12a4 4 0 0 1 4-4h10l18 18-14 14L8 22z" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}" stroke-linejoin="round"/><circle cx="17" cy="17" r="2.4" fill="${c}"/></svg>`,
};

// New: a small flag/marker badge layered onto whichever node is a query's
// root, since QueryExpr has no dedicated terminal "output" node kind the way
// the reference's BGP `out_*` steps do.
const ROOT_BADGE_ICON = (c) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48"><path d="M14 12h20v9c0 8-4 13-10 15-6-2-10-7-10-15z" fill="${ICON_FILL}" stroke="${c}" stroke-width="${ICON_STROKE}"/><path d="M18 38h12M21 42h6" stroke="${c}" stroke-width="${ICON_STROKE}" stroke-linecap="round"/></svg>`;

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

function svgDataUri(svg) {
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

function categoryOf(kind) {
  return KIND_CATEGORY[kind] || 'derive';
}

function categoryColors(name) {
  const cat = CATEGORIES[name] || CATEGORIES.derive;
  return isDarkMode() ? cat.dark : cat.light;
}

function categoryIconDataUri(name) {
  const { border } = categoryColors(name);
  const icon = ICONS[name] || ICONS.derive;
  return svgDataUri(icon(border));
}

function rootBadgeIconDataUri() {
  const { border } = isDarkMode() ? ROOT_BADGE.dark : ROOT_BADGE.light;
  return svgDataUri(ROOT_BADGE_ICON(border));
}

// Mirror the palette into CSS custom properties so plain-DOM chrome (legend,
// side panel chips) can use var(--cat-<name>-bg/border) instead of
// duplicating these hex values in index.html's <style>. Cytoscape's own
// stylesheet reads the JS tables directly (categoryColors/categoryIconDataUri)
// since var() support in a vendored cytoscape build isn't something to rely on.
function applyPaletteToCssVars() {
  const root = document.documentElement.style;
  for (const [name, cat] of Object.entries(CATEGORIES)) {
    const c = isDarkMode() ? cat.dark : cat.light;
    root.setProperty(`--cat-${name}-bg`, c.bg);
    root.setProperty(`--cat-${name}-border`, c.border);
  }
  root.setProperty('--cat-output-border', (isDarkMode() ? ROOT_BADGE.dark : ROOT_BADGE.light).border);
}

applyPaletteToCssVars();
if (window.matchMedia) {
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', applyPaletteToCssVars);
}
