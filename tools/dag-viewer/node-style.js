// Category table for QueryExpr node kinds — the single source of truth for
// how the viewer colors and labels each `DagNode.kind`. Edit this file to
// reclassify a kind or retune a palette; nothing else in index.html needs to
// change.
//
// Also covers the 7 post-ASAP-only SummaryDagNode kinds (KeepPreAsap,
// SummaryAgg, SummaryJoin, SummarySubtract, SummaryDelete, SummaryEstimate,
// SummaryMerge) that appear in the post-ASAP lane — see the `summary`
// category below.
//
// Issue #187: this table's kind list is kept in exact sync with the literal
// `&'static str` kind tags `build_no_recheck`/`summary_shape` emit in
// crates/types/src/dag_export.rs — NOT with the older `QueryExpr`-algebra
// design doc (old_docs/docs/l2-intent-algebra.md), which used names
// (`InfoJoin`, `Ref`/`LetBinding`, `Window`/`WindowFunc`, `Distinct`,
// `Merge`, plain `Sample`) that predate a rename/restructuring of the actual
// IR and no longer correspond to anything `DagNode.kind` produces at
// runtime. A kind name here that doesn't appear in dag_export.rs's `build_*`
// match arms is dead weight (or worse, silently wrong); a `DagNode.kind`
// dag_export.rs can emit that isn't a key here silently falls back to
// `derive` via `categoryOf`'s `||` — see the categorization rationale below
// for how each of the 23 pre-ASAP + 7 post-ASAP kinds was placed, especially
// the ones issue #187 called out by name.

// kind (DagNode.kind from crates/types/src/dag_export.rs) -> category name.
const KIND_CATEGORY = {
  // ── data — leaves that introduce a value, nothing flows in ─────────────
  Scan: 'data',
  PromqlScalarBridge: 'data', // a scalar literal sitting in an operator-tree position (issue #220) — no QueryExpr child of its own; a leaf like Scan, not a transform.
  EvalTimestamp: 'data',
  CurrentTimestamp: 'data',

  // ── filter — narrows rows by a boolean predicate ────────────────────────
  Filter: 'filter',

  // ── sample — narrows *series*, but not by a predicate (issue #187) ──────
  // PromqlSeriesSample (`limitk`/`limit_ratio`) keeps a deterministic subset
  // of whole series per group. Its own doc is explicit that it is "not a
  // ranking (TopK) and not a reduction" — and it's equally not a Filter:
  // nothing here is a boolean predicate over row contents, it's a
  // selection-by-quota. Grouping it with Filter (the old mapping) implied it
  // narrows rows the same way a WHERE clause does, which overstates the
  // similarity; giving it its own category keeps that distinction visible.
  PromqlSeriesSample: 'sample',

  // ── derive — transforms or enriches columns; child's rows pass through 1:1
  Project: 'derive',
  PromqlRelabel: 'derive',
  // PromqlInfoEnrich (issue #187, was "InfoJoin" bucketed with `join`): it
  // has exactly ONE QueryExpr child (`child`), unlike Join's two — the
  // "other side" it enriches from (an info metric matched by `selector`) is
  // never a QueryExpr/DagNode at all, it's resolved at runtime by the
  // post-ASAP binder. So there is no second relation in this graph for it to
  // "join" the way Join or SetOp genuinely combine two DAG inputs. What it
  // actually does — graft extra label columns onto rows that pass through
  // unchanged, same shape of operation as PromqlRelabel's column rewrite —
  // is a `derive`, not a `join`.
  PromqlInfoEnrich: 'derive',
  PromqlVectorFromScalar: 'derive',
  PromqlScalarFromVector: 'derive',
  BinaryOp: 'derive',

  // ── aggregate — groups and reduces (fewer rows out than in) ─────────────
  Aggregate: 'aggregate',

  // ── window — reads or positions a scoped window of rows/time ────────────
  TimeRange: 'window',
  PromqlSubquery: 'window',
  // TimeShift (issue #187 follow-up, not in the original 4 examples but
  // caught by the "review every other kind too" instruction): the old
  // mapping put it in `derive` ("transforms values"), but its own doc says
  // it "moves *when* `child` is evaluated... but leaves its schema
  // unchanged" — no column is transformed at all, only the temporal window
  // the rest of the plan reads from shifts. That's the same "scoped window
  // of time" concept TimeRange/PromqlSubquery represent, not a value
  // derivation, so it belongs here instead.
  TimeShift: 'window',
  SQLWindowFunc: 'window',

  // ── join — genuinely combines two DAG inputs on a predicate ─────────────
  Join: 'join',

  // ── set — enforces or computes set semantics on rows already gathered ───
  // Dedup (SQL DISTINCT, formerly labeled "Distinct" here) and SetOp
  // (UNION/INTERSECT/EXCEPT) both make a relation behave like a *set*
  // (eliminate duplicates, or combine two relations using set-theoretic
  // membership) rather than an arbitrary bag operation.
  Dedup: 'set',
  SetOp: 'set',

  // ── combine — n-ary fan-in with no set semantics (issue #187) ───────────
  // Concat (formerly labeled "Merge" here, and lumped into `set` with
  // Dedup/SetOp) is an *exact*, n-ary UNION ALL: rows are concatenated,
  // never deduplicated, and its own doc explicitly contrasts it with SetOp
  // ("SQL's UNION/INTERSECT/EXCEPT are QueryExpr::SetOp, not this"). Lumping
  // it with `set` implied it carries the same dedup/set-theoretic semantics
  // SetOp and Dedup do, which is exactly backwards — Concat is pure
  // branch-fan-in (ROLLUP/CUBE grouping-set branches, histogram_quantiles
  // branches, sharded/fan-in plans), so it gets its own category instead.
  Concat: 'combine',

  // ── sort — orders or caps rows, doesn't change which columns exist ──────
  Sort: 'sort',
  Limit: 'sort',

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
  // Post-ASAP only (post-ASAP lane): every other category
  // above is a saturated, hand-picked hue for a pre-ASAP QueryExpr operator.
  // `summary` is deliberately plain neutral gray instead of another hue —
  // partly because an 11th saturated color starts getting hard to
  // distinguish at a glance from its 10 neighbors (data's blue and sort's
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
