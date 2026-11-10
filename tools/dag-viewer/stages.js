// DOM-free logic of the Stage Viewer: checking an `asap-stage-pipeline/v2`
// document (written by the `asap_planner` devtool), the selected plan's row,
// and each drawn DAG (Stage 0's and Stage 3's plan) as cytoscape elements
// with readable labels.
// app.js draws them; test_viewer.py runs this file headless.
const STAGE_PIPELINE_FORMAT = 'asap-stage-pipeline/v2';

// A stage `run_until` stopped before, or one after a failed stage, is null;
// the viewer shows it empty.
function fillStagesNotRun(doc) {
  if (!doc || typeof doc !== 'object') return doc;
  const empty = {
    stage1_logical_asap_optimization: { pruned_candidates: [] },
    stage2_physical_asap_optimization: { physical_candidates: [], pruned_candidates: [] },
    stage3_plan_selection: { plan: null, materialized_views: [], pruned_candidates: [] },
  };
  Object.entries(empty).forEach(([stage, section]) => { if (doc[stage] === null) doc[stage] = section; });
  return doc;
}

function isStagePipelineDocument(doc) {
  return !!doc && typeof doc === 'object' && doc.format === STAGE_PIPELINE_FORMAT;
}

// `{"NonASAP": {"Scan": {...}}}` → ["Scan", {...}]; a unit variant has no body.
function stageOperatorParts(operator) {
  const inner = operator && typeof operator === 'object' ? Object.values(operator)[0] : null;
  if (typeof inner === 'string') return [inner, {}];
  if (!inner || typeof inner !== 'object') return ['unknown', {}];
  const [kind, body] = Object.entries(inner)[0] || ['unknown', {}];
  return [kind, body && typeof body === 'object' && !Array.isArray(body) ? body : { value: body }];
}

// Node ids a logical operator reads: its inputs, then the operators its
// scalar expressions read. These are the operator fields of the IR's
// `NonASAPOp` and `ASAPOp` that hold an input (crates/types/src/ir).
const STAGE_CHILD_FIELDS = ['child', 'children', 'left', 'right', 'lhs', 'rhs', 'summary_input', 'outer', 'inner'];

// Operator node ids a scalar expression reads: `scalar(v)`, scalar
// subqueries, EXISTS and IN subqueries.
const STAGE_SCALAR_REF_FIELDS = ['PromqlScalarFromVector', 'ScalarSubquery', 'subquery'];

function stageScalarRefs(value, ids = []) {
  if (!value || typeof value !== 'object') return ids;
  Object.entries(value).forEach(([field, inner]) => {
    if (STAGE_SCALAR_REF_FIELDS.includes(field) && typeof inner === 'number') ids.push(inner);
    else stageScalarRefs(inner, ids);
  });
  return ids;
}

function stageLogicalChildren(body) {
  const ids = [];
  STAGE_CHILD_FIELDS.forEach((field) => {
    [].concat(body[field] ?? []).forEach((id) => { if (typeof id === 'number') ids.push(id); });
  });
  // With its field name, so a body that is itself a scalar reference (e.g.
  // `PromqlVectorFromScalar`'s `{"PromqlScalarFromVector": id}`) is seen.
  Object.entries(body).forEach(([field, value]) => { if (!STAGE_CHILD_FIELDS.includes(field)) stageScalarRefs({ [field]: value }, ids); });
  return ids;
}

// A scalar expression as short text: `time()`, `scalar(#3)` (node 3),
// literals as their value; other forms by name.
function stageScalarText(expr) {
  if (typeof expr === 'string') return { EvalTimestamp: 'time()', CurrentTimestamp: 'now()' }[expr] || expr;
  const [kind, body] = Object.entries(expr)[0];
  switch (kind) {
    case 'Literal': return typeof body === 'object' && body ? String(Object.values(body)[0]) : String(body);
    case 'PromqlScalarFromVector': return `scalar(#${body})`;
    case 'ScalarSubquery': return `(subquery #${body})`;
    case 'Negative': return `-${stageScalarText(body.expr)}`;
    case 'Arithmetic': return `(${stageScalarText(body.left)} ${typeof body.op === 'string' ? body.op : Object.keys(body.op)[0]} ${stageScalarText(body.right)})`;
    default: return `${kind}(…)`;
  }
}

// One stage DAG in one shape for both kinds. A logical DAG (Stage 0) is
// flat: node i is `nodes[i]`, its inputs are ids inside its operator, and
// each query's root is `{"Operator": id}` or `{"Scalar": expr}`. A physical
// DAG (Stage 3's plan) has node ids, an edge list,
// operator roots in `roots`, and standalone scalar queries in
// `scalar_roots` as `[query position, expr]`, so query i is not `roots[i]`.
// `queries` holds, per workload query, `{ node: id }` or `{ scalar: expr }`.
function stageDag(dag, physical) {
  if (physical) {
    const nodes = dag.nodes.map((node) => {
      const [kind, body] = stageOperatorParts(node.payload);
      return {
        id: node.id, kind, body, schema: node.output_schema, guarantee: node.guarantee,
        lifetime: node.lifetime, timing: node.output_state ? node.output_state.timing : undefined, raw: node,
      };
    });
    const edges = dag.edges.map((edge) => ({ producer: edge.producer, consumer: edge.consumer, raw: edge }));
    const scalars = new Map((dag.scalar_roots || []).map(([position, expr]) => [position, expr]));
    const queries = [];
    let next = 0;
    for (let index = 0; next < dag.roots.length || scalars.has(index); index += 1) {
      queries.push(scalars.has(index) ? { scalar: scalars.get(index) } : { node: dag.roots[next++] });
    }
    return { nodes, edges, queries };
  }
  const edges = [];
  const nodes = dag.nodes.map((node, id) => {
    const [kind, body] = stageOperatorParts(node.operator);
    stageLogicalChildren(body).forEach((producer) => edges.push({ producer, consumer: id, raw: null }));
    return {
      id, kind, body, schema: node.schema, guarantee: node.guarantee, raw: node,
    };
  });
  const queries = dag.roots.map((root) => ('Scalar' in root ? { scalar: root.Scalar } : { node: root.Operator }));
  return { nodes, edges, queries };
}

function validateLogicalDag(dag, where, queryCount) {
  if (!dag || !Array.isArray(dag.nodes) || !Array.isArray(dag.roots)) return [`${where}: dag needs nodes and roots`];
  const errors = [];
  dag.nodes.forEach((node, id) => {
    if (!node || !node.operator) { errors.push(`${where}: node ${id} has no operator`); return; }
    stageLogicalChildren(stageOperatorParts(node.operator)[1]).forEach((child) => {
      if (!(child >= 0 && child < dag.nodes.length)) errors.push(`${where}: node ${id} reads missing node ${child}`);
    });
  });
  if (dag.roots.length !== queryCount) errors.push(`${where}: ${dag.roots.length} roots for ${queryCount} workload queries`);
  dag.roots.forEach((root, index) => {
    if (!root || typeof root !== 'object' || !('Operator' in root || 'Scalar' in root)) errors.push(`${where}: root #${index} is malformed`);
    else if ('Operator' in root && !(root.Operator >= 0 && root.Operator < dag.nodes.length)) errors.push(`${where}: root ${root.Operator} does not name a node`);
  });
  return errors;
}

function validatePhysicalDag(dag, where, queryCount) {
  if (!dag || !Array.isArray(dag.nodes) || !Array.isArray(dag.edges) || !Array.isArray(dag.roots)) {
    return [`${where}: dag needs nodes, edges and roots`];
  }
  const errors = [];
  const ids = new Set();
  dag.nodes.forEach((node, index) => {
    if (!node || typeof node.id !== 'number') { errors.push(`${where}: node #${index} has no numeric id`); return; }
    if (ids.has(node.id)) errors.push(`${where}: duplicate node id ${node.id}`);
    ids.add(node.id);
    if (!node.payload) errors.push(`${where}: node ${node.id} has no payload`);
  });
  dag.edges.forEach((edge, index) => {
    if (!edge || !ids.has(edge.producer) || !ids.has(edge.consumer)) errors.push(`${where}: edge #${index} names a missing producer or consumer`);
  });
  if (dag.scalar_roots !== undefined && !Array.isArray(dag.scalar_roots)) errors.push(`${where}: scalar_roots must be an array`);
  const scalars = Array.isArray(dag.scalar_roots) ? dag.scalar_roots : [];
  if (dag.roots.length + scalars.length !== queryCount) {
    errors.push(`${where}: ${dag.roots.length} roots and ${scalars.length} scalar roots for ${queryCount} workload queries`);
  }
  // `scalar_roots` holds [query position, expr], in increasing position.
  let previous = -1;
  scalars.forEach((entry, index) => {
    const position = Array.isArray(entry) ? entry[0] : undefined;
    if (!Number.isInteger(position) || position <= previous || position >= queryCount) {
      errors.push(`${where}: scalar root #${index} needs a query position after ${previous} and below ${queryCount}`);
      return;
    }
    previous = position;
    stageScalarRefs(entry[1]).forEach((id) => {
      if (!ids.has(id)) errors.push(`${where}: scalar root of query #${position + 1} reads missing node ${id}`);
    });
  });
  dag.roots.forEach((root, index) => {
    if (typeof root !== 'number') errors.push(`${where}: root #${index} is not a node id`);
    else if (!ids.has(root)) errors.push(`${where}: root ${root} does not name a node`);
  });
  return errors;
}

const isPlainObject = (value) => !!value && typeof value === 'object' && !Array.isArray(value);

// A pruning reason of Stage 2 or Stage 3 (a serialized `PruneReason`): a
// snake_case string, e.g. "usage_not_costable" as "usage not costable", or
// `{"dominated_by": id}` as "dominated by C<id>", the physical candidate
// that dominates it.
function prunedReasonText(reason) {
  if (typeof reason === 'string') return reason.replace(/_/g, ' ');
  if (isPlainObject(reason) && 'dominated_by' in reason) return `dominated by C${reason.dominated_by}`;
  return compactWire(reason);
}

// Stage 3's plan, its cost and its materialized views, whose node ids are
// the plan's. Without a plan (selection failed or did not run) there is no
// view. `cost` is null when the plan could not be costed.
function validatePlanSelection(stage3, queryCount) {
  const at = 'stage3_plan_selection';
  const views = stage3.materialized_views === undefined ? [] : stage3.materialized_views;
  if (!Array.isArray(views)) return [`${at}.materialized_views must be an array`];
  if (stage3.plan === null || stage3.plan === undefined) return views.length ? [`${at}.materialized_views must be empty without a plan`] : [];
  const errors = validatePhysicalDag(stage3.plan, `${at}.plan`, queryCount);
  if (errors.length) return errors;
  const nodeIds = new Set(stage3.plan.nodes.map((node) => node.id));
  const cost = stage3.cost;
  if (cost !== null && cost !== undefined) {
    // `cost_per_second` is null under cost model 3 (own cluster), which has no unit costs.
    if (!isPlainObject(cost) || (cost.cost_per_second !== null && !Number.isFinite(cost.cost_per_second))) errors.push(`${at}.cost: cost_per_second must be a number or null`);
    Object.keys((cost && cost.per_node) || {}).forEach((key) => {
      if (!nodeIds.has(Number(key))) errors.push(`${at}.cost.per_node names missing node ${key}`);
    });
  }
  views.forEach((view, index) => {
    const where = `${at}.materialized_views[${index}]`;
    if (!isPlainObject(view)) { errors.push(`${where} must be an object`); return; }
    if (!nodeIds.has(view.node_id)) errors.push(`${where}: node_id must name a node of the plan`);
    if (!VIEW_REFRESHES.includes(view.refresh)) errors.push(`${where}: refresh must be one of ${VIEW_REFRESHES.join(', ')}`);
    if (!view.coverage || typeof view.coverage.definition_id !== 'string') errors.push(`${where}: coverage.definition_id must be a string`);
    if (!view.inputs || !Array.isArray(view.inputs.raw_data_windows) || !Array.isArray(view.inputs.materialized_views)) {
      errors.push(`${where}: inputs needs raw_data_windows and materialized_views arrays`);
    }
    if (!(typeof view.retention_ms === 'number' && view.retention_ms >= 0)) errors.push(`${where}: retention_ms must be a non-negative number`);
  });
  return errors;
}

// The checks of the pruned candidates of Pass 2, Stage 2 and Stage 3 and of
// Stage 2's physical candidates (the memo view reads them), as messages.
// Physical candidates are numbered by their position; a physical edge's
// `producer` and a `dominated_by` reason index them.
function validatePrunedAndPhysical(stage1, stage2, stage3) {
  const pass2 = stage1.pruned_candidates === undefined ? [] : stage1.pruned_candidates;
  const physical = stage2.physical_candidates;
  const pruned2 = stage2.pruned_candidates;
  const pruned3 = stage3.pruned_candidates === undefined ? [] : stage3.pruned_candidates;
  if (!Array.isArray(pass2)) return ['stage1_logical_asap_optimization.pruned_candidates must be an array'];
  if (!Array.isArray(physical) || !Array.isArray(pruned2)) return ['stage2_physical_asap_optimization: physical_candidates and pruned_candidates must be arrays'];
  if (!Array.isArray(pruned3)) return ['stage3_plan_selection.pruned_candidates must be an array'];
  const errors = [];
  const isIndex = (v) => Number.isInteger(v) && v >= 0;
  const isCandidate = (v) => isIndex(v) && v < physical.length;
  const isMemoCandidate = (c) => !!c && isIndex(c.group) && isIndex(c.index_in_group);
  const hasProperties = (p) => !!p && typeof p.timing === 'string' && p.lifetime !== undefined;
  const isReason = (r) => typeof r === 'string' || (isPlainObject(r) && (!('dominated_by' in r) || isCandidate(r.dominated_by)));
  pass2.forEach((p, k) => {
    if (!isMemoCandidate(p) || !Array.isArray(p.causes) || !p.causes.length || !p.causes.every((cause) => cause
      && Array.isArray(cause.through_pruned_candidates) && cause.through_pruned_candidates.every(isMemoCandidate)
      && (typeof cause.root === 'string' || isPlainObject(cause.root)) && typeof cause.text === 'string')) {
      errors.push(`stage1_logical_asap_optimization.pruned_candidates[${k}]: needs group, index_in_group and causes, each with through_pruned_candidates, root and text`);
    }
  });
  physical.forEach((c, k) => {
    const at = `stage2_physical_asap_optimization.physical_candidates[${k}]`;
    if (!c || c.id !== k || !isMemoCandidate(c) || !hasProperties(c.properties) || !Array.isArray(c.inputs)) {
      errors.push(`${at}: needs id ${k}, group, index_in_group, properties {timing, lifetime} and inputs`);
      return;
    }
    c.inputs.forEach((input, i) => {
      if (!input || !isIndex(input.group) || !Array.isArray(input.edges)
        || !input.edges.every((e) => e && isCandidate(e.producer) && typeof e.satisfies_physical_requirement === 'boolean'
          && typeof e.selected === 'boolean' && (e.rejection === null || typeof e.rejection === 'string'))) {
        errors.push(`${at}.inputs[${i}]: needs group and edges whose producer is a physical candidate`);
      }
    });
  });
  [['stage2_physical_asap_optimization', pruned2], ['stage3_plan_selection', pruned3]].forEach(([stage, pruned]) => pruned.forEach((p, k) => {
    if (!isMemoCandidate(p) || !hasProperties(p.properties) || !isReason(p.reason)) {
      errors.push(`${stage}.pruned_candidates[${k}]: needs group, index_in_group, properties and a reason (dominated_by naming a physical candidate)`);
    }
  }));
  return errors;
}

// Every shape problem, as readable strings; empty means the document can be
// shown. The viewer refuses a document with problems rather than guessing.
function validateStagePipeline(doc) {
  if (!isStagePipelineDocument(doc)) return [`format must be ${JSON.stringify(STAGE_PIPELINE_FORMAT)}`];
  const queries = doc.workload && doc.workload.queries;
  if (!Array.isArray(queries) || queries.length === 0) return ['workload.queries must be a non-empty array'];
  const stage0 = doc.stage0_query_language_frontends;
  const stage1 = doc.stage1_logical_asap_optimization;
  const stage2 = doc.stage2_physical_asap_optimization;
  const stage3 = doc.stage3_plan_selection;
  const missing = [
    ['stage0_query_language_frontends', stage0], ['stage1_logical_asap_optimization', stage1],
    ['stage2_physical_asap_optimization', stage2], ['stage3_plan_selection', stage3],
  ].filter(([, stage]) => !stage || typeof stage !== 'object').map(([name]) => `${name} is missing`);
  if (missing.length) return missing;
  const errors = [];
  if (doc.accuracy_guarantees !== undefined && !ACCURACY_GUARANTEE_MODES.includes(doc.accuracy_guarantees)) {
    errors.push(`accuracy_guarantees must be ${ACCURACY_GUARANTEE_MODES.map((m) => JSON.stringify(m)).join(' or ')}`);
  }
  errors.push(...validateLogicalDag(stage0.dag, 'stage0_query_language_frontends', queries.length));
  errors.push(...validatePrunedAndPhysical(stage1, stage2, stage3));
  errors.push(...validatePlanSelection(stage3, queries.length));
  errors.push(...validateDataWorkload(doc.workload.data_workload));
  return errors;
}

// A query's accuracy target (`AccuracyTarget` JSON: `accuracy_metric`,
// `max_error`, `failure_probability`, `accuracy_scope`) as one line, e.g. "ε=0.01, δ=0.001
// (rank)", "ε=0.01 (metric not set; default relative)" or "ε=0.05, δ=0.05
// (relative; groups ≥ 5% of records)"; "exact" for zero error with
// certainty; "" for a malformed target. `defaultMetric` is the query's default
// metric the document shows for a target without one. The scope is shown
// unless it is the default, the whole result.
function accuracyText(target, defaultMetric) {
  if (!target || typeof target !== 'object') return '';
  if (target.max_error === 0 && target.failure_probability === 0) return 'exact';
  const delta = target.failure_probability !== undefined && target.failure_probability !== null
    ? `, δ=${target.failure_probability}` : '';
  const metric = target.accuracy_metric || `metric not set${defaultMetric ? `; default ${defaultMetric}` : ''}`;
  const scope = scopeText(target.accuracy_scope);
  return `ε=${target.max_error}${delta} (${metric}${scope ? `; ${scope}` : ''})`;
}

// An `AccuracyScope` as words: "" for the whole result (the default),
// "per value", or "groups ≥ 5% of records" for
// `{"groups_with_record_share_at_least": 0.05}`.
function scopeText(scope) {
  if (scope === 'per_value') return 'per value';
  const share = scope && typeof scope === 'object' ? scope.groups_with_record_share_at_least : undefined;
  if (typeof share === 'number') return `groups ≥ ${+(share * 100).toPrecision(6)}% of records`;
  return '';
}

// What Stage 3 accepted per query (`accepted_accuracy_per_query`), as
// "q1: …; q2: …"; null when the document has none.
function acceptedText(queries) {
  if (!Array.isArray(queries)) return null;
  const parts = queries.map((q, index) => (q && q.summary ? `q${index + 1}: ${q.summary}` : null)).filter(Boolean);
  return parts.length ? parts.join('; ') : null;
}

// Stage 3's outcome as the plan list's one row: the selected plan with its
// cost per second (null when not costed or under cost model 3) and the
// accuracy it accepted, or selection's error; null when Stage 3 did not run.
function selectedPlanRow(doc) {
  const stage3 = doc.stage3_plan_selection || {};
  if (stage3.plan) {
    return { status: 'selected', total: stage3.cost ? stage3.cost.cost_per_second : null, accepted: acceptedText(stage3.accepted_accuracy_per_query), reason: null };
  }
  if (stage3.error) return { status: 'no plan', total: null, accepted: null, reason: stage3.error };
  return null;
}

// The plan list's header: why Stage 1 (Pass 1 or Pass 2) or Stage 2 failed,
// why selection failed, and the selection's warnings (e.g. a very large
// peak memory with no declared memory capacity); "" when there is none.
function planHeader(doc) {
  const stage1 = doc.stage1_logical_asap_optimization || {};
  const stage2 = doc.stage2_physical_asap_optimization || {};
  const stage3 = doc.stage3_plan_selection || {};
  return [
    stage1.error ? `Stage 1 failed: ${stage1.error}` : '',
    stage2.error ? `Stage 2 failed: ${stage2.error}` : '',
    stage3.error ? `selection failed: ${stage3.error}` : '',
    ...(stage3.warnings || []).map((warning) => `warning: ${warning}`),
  ].filter(Boolean).join(' · ');
}

// The candidates at each stage as one line: the logical candidates of the
// Pass 2 memo and how many Pass 2 pruned, the physical candidates and how
// many Stage 2 and Stage 3 pruned, and whether a plan was selected.
function candidateCountsText(doc) {
  const pass2 = ((doc.memo || {}).steps || []).find((step) => step.step === 'pass2');
  const logical = pass2 ? pass2.groups.reduce((n, g) => n + g.candidate_count, 0) : 0;
  const count = (section) => ((section || {}).pruned_candidates || []).length;
  const physical = ((doc.stage2_physical_asap_optimization || {}).physical_candidates || []).length;
  return `Candidates per stage: ${logical} logical (${count(doc.stage1_logical_asap_optimization)} pruned by Pass 2)`
    + ` → ${physical} physical (${count(doc.stage2_physical_asap_optimization)} pruned by Stage 2, ${count(doc.stage3_plan_selection)} by Stage 3)`
    + ` → ${(doc.stage3_plan_selection || {}).plan ? 'plan selected' : 'no plan selected'}`;
}

// Whether Stage 3 checked accuracy guarantees (`accuracy_guarantees`), for
// the plan list's header; "" for a document that does not say.
const ACCURACY_GUARANTEE_MODES = ['ignored', 'enforced'];
function accuracyGuaranteesText(doc) {
  return {
    ignored: 'accuracy guarantees ignored: every output accepted, candidates compared on cost alone',
    enforced: 'accuracy guarantees enforced',
  }[doc.accuracy_guarantees] || '';
}

// The search behind the document and the size and times of Stage 3's
// selection MILP, for the plan list's header, e.g. "candidate-connection
// search · MILP 88 variables, 126 constraints · construction 1 ms, solve
// 23 ms". "" for a document that names no search.
function searchText(doc) {
  const name = { 'candidate-connections': 'candidate-connection search' }[doc.search];
  if (!name) return '';
  const stage3 = doc.stage3_plan_selection || {};
  if (typeof stage3.milp_variables !== 'number') return name;
  return `${name} · MILP ${stage3.milp_variables} variables, ${stage3.milp_constraints} constraints`
    + ` · construction ${stage3.milp_construction_ms} ms, solve ${stage3.milp_solve_ms} ms`;
}

function compactWire(value) {
  if (value === null || value === undefined) return 'none';
  if (typeof value !== 'object') return String(value);
  if (Array.isArray(value)) return value.map(compactWire).join(', ') || 'none';
  const keys = Object.keys(value);
  if (keys.length === 1) {
    const inner = value[keys[0]];
    return inner === null || (typeof inner === 'object' && !Array.isArray(inner) && Object.keys(inner).length === 0)
      ? keys[0]
      : `${keys[0]}(${compactWire(inner)})`;
  }
  return keys.map((key) => `${key}=${compactWire(value[key])}`).join(', ');
}

function wireSource(source) {
  if (source && source.TimeSeries && source.TimeSeries.metric) return source.TimeSeries.metric;
  if (source && source.Table && source.Table.table_ref) return source.Table.table_ref;
  return compactWire(source);
}

function wireColumn(index, schema) {
  const field = schema && Array.isArray(schema.fields) ? schema.fields[index] : null;
  return field && field.name ? field.name : `col[${index}]`;
}

function wireGrouping(reduction, schema) {
  if (reduction === 'PerEntity') return 'per series';
  if (reduction && Array.isArray(reduction.Reduce)) return `group by ${reduction.Reduce.map((key) => wireColumn(key, schema)).join(', ') || 'all rows'}`;
  return reduction === undefined ? null : `reduction: ${compactWire(reduction)}`;
}

function wirePartition(keys, schema) {
  const list = Array.isArray(keys) ? keys : keys && Array.isArray(keys.keys) ? keys.keys : [];
  if (list.length === 0) return null;
  return `${keys.without ? 'per group without' : 'per'} ${list.map((key) => wireColumn(key, schema)).join(', ')}`;
}

function formatTiming(timing) {
  const normalized = String(timing || '').replace(/_/g, '').toLowerCase();
  if (normalized === 'ingestiontime') return 'ingestion time';
  if (normalized === 'querytime') return 'query time';
  return String(timing || 'unknown');
}

// A summary type (`FieldDataType` JSON), e.g. `{"Sketch": "Kll"}` as "Kll"
// and `{"ExactAggregate": ["Sum", "Sum"]}` as "exact Sum". θ is not part
// of the type: the memo view shows each candidate's.
function summaryTypeText(summaryType) {
  if (summaryType && typeof summaryType.Sketch === 'string') return summaryType.Sketch;
  if (summaryType && Array.isArray(summaryType.ExactAggregate)) return `exact ${summaryType.ExactAggregate[0]}`;
  return compactWire(summaryType);
}

// Box text for an operator (`{"NonASAP": {...}}` or `{"ASAP": {...}}`)
// from its concrete fields; `inputSchema` names the columns it reads. The
// details panel shows the full operator.
function stageNodeLines(operator, inputSchema) {
  const [kind, op] = stageOperatorParts(operator);
  const lines = [kind];
  const grouping = wireGrouping(op.reduction, inputSchema);
  switch (kind) {
    case 'Scan': lines.push(`source: ${wireSource(op.source)}`); break;
    case 'TimeRange': lines.push(`range: ${op.range ? op.range.secs : '?'}s`); break;
    case 'Aggregate':
      (op.measures || []).forEach((measure) => {
        const args = [measure.col === null || measure.col === undefined ? '' : wireColumn(measure.col, inputSchema)]
          .concat(['q', 'k'].filter((key) => key in measure).map((key) => `${key}=${measure[key]}`))
          .filter(Boolean);
        lines.push(`measure: ${measure.kind}(${args.join(', ')})`);
      });
      if (grouping) lines.push(grouping);
      break;
    case 'Filter': lines.push(`where: ${compactWire(op.pred)}`.slice(0, 80)); break;
    case 'Sort':
      lines.push(`sort: ${(op.keys || []).map((key) => `${key.expr && typeof key.expr.Column === 'number' ? wireColumn(key.expr.Column, inputSchema) : compactWire(key.expr)} ${key.ascending ? 'asc' : 'desc'}`).join(', ')}`);
      if (wirePartition(op.partition_by, inputSchema)) lines.push(wirePartition(op.partition_by, inputSchema));
      break;
    case 'Limit':
      lines.push(`rows: ${op.n === null || op.n === undefined ? 'all' : op.n}${op.offset ? `, offset ${op.offset}` : ''}`);
      if (wirePartition(op.partition_by, inputSchema)) lines.push(wirePartition(op.partition_by, inputSchema));
      break;
    case 'SummaryAgg':
      lines.push(`summary: ${summaryTypeText(op.summary_type)}`);
      if (grouping) lines.push(grouping);
      break;
    case 'SummaryEstimate': lines.push(`query: ${compactWire(op.query)}`); break;
    case 'SummaryTumblingWindowChain':
      lines.push(`summary: ${summaryTypeText(op.summary_type)}`);
      lines.push(`tumbling windows of ${durationText(op.tumbling_window_width_ms)}`);
      if (grouping) lines.push(grouping);
      break;
    default: break;
  }
  return lines.slice(0, 4);
}

// Per node id of a stage DAG (`stageDag`'s shape), the indices of the
// queries whose root reaches it: its root, or a node its root reads,
// directly or through other nodes; a standalone scalar query reaches the
// operators it reads and everything below them.
function stageQueryReach(lane) {
  const producers = new Map();
  lane.edges.forEach((edge) => {
    if (!producers.has(edge.consumer)) producers.set(edge.consumer, []);
    producers.get(edge.consumer).push(edge.producer);
  });
  const reach = new Map();
  lane.queries.forEach((query, index) => {
    const stack = 'node' in query ? [query.node] : stageScalarRefs(query.scalar);
    const seen = new Set(stack);
    while (stack.length) {
      const id = stack.pop();
      if (!reach.has(id)) reach.set(id, []);
      reach.get(id).push(index);
      (producers.get(id) || []).forEach((p) => { if (!seen.has(p)) { seen.add(p); stack.push(p); } });
    }
  });
  return reach;
}

// A physical node's timing as its lane class, which colors its fill:
// `ingestionTime`, `queryTime`, or "" without a timing (a logical node).
function timingClass(timing) {
  const text = timing === undefined || timing === null ? '' : formatTiming(timing);
  if (text === 'ingestion time') return 'ingestionTime';
  if (text === 'query time') return 'queryTime';
  return '';
}

// A stage DAG (`dag`, as the document writes it) as cytoscape elements:
// one node per operator (`n<id>`), one `Scalar` node per standalone scalar
// query (`s<query position>`) over the nodes it reads, and one edge per
// input. Each node lists the queries whose root reaches it (`reachedBy`;
// "shared ×n" in its box when n > 1). Options: `physical` (Stage 3's plan:
// nodes show their timing), `costPerNode` (the plan's Stage 3 `per_node`
// costs), `queryIds` (the workload's query ids, to mark each query's root).
function stageLaneElements(dag, options) {
  const { physical = false, costPerNode = null, queryIds = [] } = options || {};
  const group = typeof nodeGroup === 'function' ? nodeGroup : () => 'rel';
  const lane = stageDag(dag, physical);
  const byId = new Map(lane.nodes.map((node) => [node.id, node]));
  const inputOf = new Map();
  lane.edges.forEach((edge) => { if (!inputOf.has(edge.consumer)) inputOf.set(edge.consumer, byId.get(edge.producer)); });
  const rootFor = new Map();
  lane.queries.forEach((query, index) => {
    if (!('node' in query)) return;
    if (!rootFor.has(query.node)) rootFor.set(query.node, []);
    rootFor.get(query.node).push(queryIds[index] || `query #${index + 1}`);
  });
  const queryName = (index) => queryIds[index] || `query #${index + 1}`;
  const reach = stageQueryReach(lane);

  const nodes = lane.nodes.map((node) => {
    const input = inputOf.get(node.id);
    const queries = (reach.get(node.id) || []).map(queryName);
    const lines = stageNodeLines(physical ? node.raw.payload : node.raw.operator, input && input.schema);
    const cost = costPerNode ? costPerNode[String(node.id)] : undefined;
    if (physical && node.timing !== undefined) lines.push(`⏱ ${formatTiming(node.timing)}`);
    if (cost && typeof cost.cost_per_second === 'number') {
      lines.push(`cost ${Number(cost.cost_per_second.toPrecision(3))}${cost.multiplier === 1 || cost.multiplier === undefined ? '' : ` × ${cost.multiplier}`}`);
    }
    if (queries.length > 1) lines.push(`shared ×${queries.length}`);
    const roots = rootFor.get(node.id) || [];
    if (roots.length) lines.push(`root of ${roots.join(', ')}`);
    return {
      data: {
        id: `n${node.id}`, nodeId: node.id, kind: node.kind, group: group(node.kind), label: lines.join('\n'),
        rootFor: roots, reachedBy: queries, timing: node.timing, cost, node: node.raw,
      },
      classes: timingClass(node.timing),
    };
  });
  const edges = lane.edges.map((e, index) => ({ data: { id: `e${index}`, source: `n${e.producer}`, target: `n${e.consumer}`, edge: e.raw } }));
  lane.queries.forEach((query, index) => {
    if (!('scalar' in query)) return;
    const id = `s${index}`;
    const queryId = queryName(index);
    nodes.push({
      data: {
        id, nodeId: id, kind: 'Scalar', group: group('Scalar'), rootFor: [queryId], reachedBy: [queryId], node: { Scalar: query.scalar },
        label: `Scalar\n${stageScalarText(query.scalar)}\nroot of ${queryId}`,
      },
      classes: '',
    });
    stageScalarRefs(query.scalar).forEach((producer, k) => edges.push({
      data: { id: `${id}e${k}`, source: `n${producer}`, target: id, edge: null },
    }));
  });
  return nodes.concat(edges);
}

// The element ids (nodes and edges) of `elements` that query `queryId`
// reaches: the nodes listing it in `reachedBy`, and the edges between two
// of them.
function stageQueryElementIds(elements, queryId) {
  const nodes = new Set(elements.filter((e) => !('source' in e.data) && (e.data.reachedBy || []).includes(queryId)).map((e) => e.data.id));
  const edges = elements.filter((e) => 'source' in e.data && nodes.has(e.data.source) && nodes.has(e.data.target)).map((e) => e.data.id);
  return [...nodes, ...edges];
}

// The element ids of `elements` upstream and downstream of node `nodeId`
// (an element id): the node, every node it reads or that reads it,
// directly or through others, and the edges along those paths.
function stageNeighborhoodIds(elements, nodeId) {
  const edges = elements.filter((e) => 'source' in e.data);
  const walk = (from, to) => {
    const seen = new Set([nodeId]);
    const stack = [nodeId];
    const along = [];
    while (stack.length) {
      const id = stack.pop();
      edges.filter((e) => e.data[to] === id).forEach((e) => {
        along.push(e.data.id);
        if (!seen.has(e.data[from])) { seen.add(e.data[from]); stack.push(e.data[from]); }
      });
    }
    return [...seen, ...along];
  };
  return [...new Set([...walk('source', 'target'), ...walk('target', 'source')])];
}

// The run's data workload (`workload.data_workload`, the `DataWorkload` the
// planner planned with): absent in older documents; otherwise an object
// with `sources`, one `SourceFacts` object per source.
function validateDataWorkload(dataWorkload) {
  if (dataWorkload === undefined) return [];
  const at = 'workload.data_workload';
  if (!dataWorkload || typeof dataWorkload !== 'object' || Array.isArray(dataWorkload)) return [`${at} must be an object`];
  const sources = dataWorkload.sources;
  if (!sources || typeof sources !== 'object' || Array.isArray(sources)) return [`${at}.sources must be an object`];
  return Object.entries(sources)
    .filter(([, facts]) => !facts || typeof facts !== 'object' || Array.isArray(facts))
    .map(([name]) => `${at}.sources.${name} must be an object`);
}

// How a materialized view is refreshed (`MaterializedViewRefresh`).
const VIEW_REFRESHES = ['at_ingestion', 'on_first_read'];

// Milliseconds in units, largest first: "100 ms", "15 s", "10 min",
// "9 min 40 s", "1 h".
function durationText(ms) {
  if (typeof ms !== 'number') return '—';
  if (ms === 0) return '0 s';
  const units = [[3600000, 'h'], [60000, 'min'], [1000, 's'], [1, 'ms']];
  const parts = [];
  let left = ms;
  units.forEach(([size, name]) => {
    if (left >= size) { parts.push(`${Math.floor(left / size)} ${name}`); left %= size; }
  });
  return parts.join(' ');
}

// The ids of `queries` (`{id, text}`) whose text names `source` as a whole
// word: the queries that read it, as far as their text says.
function queriesNaming(source, queries) {
  const escaped = source.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const word = new RegExp(`(^|[^A-Za-z0-9_:])${escaped}($|[^A-Za-z0-9_:])`);
  return queries.filter((query) => word.test(query.text || '')).map((query) => query.id);
}

// A data workload (`DataWorkload` JSON) as its arrival line and one row per
// source with the facts it declares; an undeclared fact is "—". `queries`
// (`{id, text}`) fill each row's `readBy`.
function dataWorkloadView(dataWorkload, queries) {
  if (!dataWorkload || typeof dataWorkload !== 'object') return { arrival: 'none declared', rows: [] };
  const arrival = dataWorkload.arrival;
  const [kind, body] = typeof arrival === 'string' ? [arrival, {}] : arrival && typeof arrival === 'object' ? Object.entries(arrival)[0] || ['unknown', {}] : ['unknown', {}];
  const rate = body && typeof body.rows_per_second === 'number' ? ` · ${Number(body.rows_per_second.toPrecision(4))} rows/s` : '';
  const norms = (n) => (n ? ['l1', 'l2'].filter((k) => typeof n[k] === 'number').map((k) => `${k} ${n[k]}`).join(', ') || '—' : '—');
  const rows = Object.entries(dataWorkload.sources || {}).map(([name, facts]) => ({
    name,
    series: typeof facts.series_count === 'number' ? facts.series_count : '—',
    type: facts.metric_type || '—',
    nonNegative: (facts.column_signs || {}).value === 'non_negative' ? 'yes' : String(facts.metric_type).toLowerCase() === 'counter' ? 'yes (counter)' : '—',
    sampleInterval: facts.sample_interval === undefined || facts.sample_interval === null ? '—' : durationText(facts.sample_interval),
    streamNorms: norms(facts.stream_norms),
    maxDistinct: typeof facts.max_distinct_per_evaluation === 'number' ? facts.max_distinct_per_evaluation : '—',
    distinctValuesPerLabel: Object.entries(facts.distinct_values_per_label || {}).map(([label, count]) => `${label} ${count}`).join(', ') || '—',
    distinctValuesPerLabelSet: (Array.isArray(facts.distinct_values_per_label_set) ? facts.distinct_values_per_label_set : []).map((set) => `(${(set.labels || []).join(', ')}) ${set.distinct_values}`).join(', ') || '—',
    readBy: queriesNaming(name, queries || []),
  }));
  return { arrival: `${kind}${rate}`, rows };
}

// The workload query ids whose root reaches node `nodeId` of a physical DAG
// (Stage 3's plan): the queries that read it, directly or through other nodes.
function planQueriesReaching(dag, nodeId, queryIds) {
  return (stageQueryReach(stageDag(dag, true)).get(nodeId) || []).map((index) => queryIds[index] || `query #${index + 1}`);
}

// The selected plan's materialized views as table rows: the plan node, its
// coverage's definition, the node's operator and summary type, how it is
// refreshed, what its refresh reads, its retention, the raw-data windows it
// scans, the node's Stage 3 cost and the queries that read it. Empty
// without a plan.
function materializedViewRows(doc) {
  const stage3 = doc.stage3_plan_selection || {};
  if (!stage3.plan) return [];
  const queryIds = doc.workload.queries.map((q) => q.id);
  const nodes = new Map(stage3.plan.nodes.map((node) => [node.id, node]));
  const perNode = (stage3.cost || {}).per_node || {};
  return (stage3.materialized_views || []).map((view) => {
    const [kind, body] = stageOperatorParts(nodes.get(view.node_id).payload);
    const cost = perNode[String(view.node_id)];
    return {
      nodeId: view.node_id,
      definition: view.coverage.definition_id,
      kind: 'summary_type' in body ? `${kind} · ${summaryTypeText(body.summary_type)}` : kind,
      refresh: view.refresh === 'at_ingestion' ? 'at ingestion' : 'on first read (materialized across evaluations)',
      inputs: `${view.inputs.raw_data_windows.length} raw-data window(s)` +
        (view.inputs.materialized_views.length ? `, views ${view.inputs.materialized_views.join(', ')}` : ''),
      retention: durationText(view.retention_ms),
      window: view.inputs.raw_data_windows.map((w) => `t−${durationText(w.end_offset_ms + w.width_ms)} … ${w.end_offset_ms ? `t−${durationText(w.end_offset_ms)}` : 't'}`).join(', ') || '—',
      cost: cost ? cost.detail || '' : '',
      readBy: planQueriesReaching(stage3.plan, view.node_id, queryIds),
    };
  });
}

// When a stage document's query runs: its recurrence (`QueryRecurrence`
// JSON), its predictability and its range evaluation, e.g. "every 20 s ·
// predictable · range of 1 h in 1 min steps"; "" when the document has none.
function queryScheduleText(query) {
  const parts = [];
  const recurrence = query.recurrence;
  const [kind, body] = typeof recurrence === 'string' ? [recurrence, null]
    : recurrence && typeof recurrence === 'object' ? Object.entries(recurrence)[0] || [null, null] : [null, null];
  if (kind === 'repeated') {
    // `fixed_interval_at` (`interval` and `evaluation_phase`, ms).
    const at = body && typeof body === 'object' ? body.fixed_interval_at : undefined;
    const interval = at ? at.interval : undefined;
    const phase = at && at.evaluation_phase ? ` at phase ${durationText(at.evaluation_phase)}` : '';
    parts.push(typeof interval === 'number' ? `every ${durationText(interval)}${phase}` : `repeated: ${compactWire(body)}`);
  } else if (kind === 'one_time') {
    parts.push(body && body.invocations > 1 ? `${body.invocations} invocations` : 'once');
    if (body && typeof body.execute_at === 'number') parts.push(`at ${body.execute_at} ms`);
  } else if (kind === 'unknown') parts.push('recurrence unknown');
  else if (kind) parts.push(compactWire(recurrence));
  const predictability = query.predictability;
  const [pkind] = typeof predictability === 'string' ? [predictability]
    : predictability && typeof predictability === 'object' ? Object.keys(predictability) : [null];
  if (pkind) parts.push(pkind.replace(/_/g, ' '));
  const range = (query.requirements || {}).range_evaluation;
  if (range && typeof range.span_ms === 'number') parts.push(`range of ${durationText(range.span_ms)} in ${durationText(range.step_ms)} steps`);
  return parts.join(' · ');
}

// A candidate's Stage 3 `usage` as one line: CPU, scans, memory, disk,
// cores and each query's latency.
function usageText(usage) {
  if (!usage || typeof usage !== 'object') return '';
  const n = (x) => (typeof x === 'number' ? Number(x.toPrecision(3)).toLocaleString('en-US') : '—');
  const latency = (usage.latency_per_query || []).map((l, i) => `q${i + 1} ${n(l.compaction_ms + l.query_ms)} ms`).join(', ');
  return [`CPU ${n(usage.cpu_ops_per_second)} ops/s`, `scans ${n(usage.scanned_bytes_per_second)} B/s`,
    `memory ${n(usage.average_memory_bytes)} B average, ${n(usage.peak_memory_bytes)} B peak`, `disk ${n(usage.disk_bytes)} B`,
    `${n(usage.peak_cpu_cores)} peak cores`].concat(latency ? [`latency ${latency}`] : []).join(' · ');
}

// The run's deployment capabilities (`DeploymentCapabilities` JSON) as one
// line: "raw data not retained · query-time results not retained · every
// summary the planner builds".
function deploymentText(capabilities) {
  if (!capabilities || typeof capabilities !== 'object') return '';
  const summaries = Array.isArray(capabilities.supported_summaries)
    ? `summaries: ${capabilities.supported_summaries.map((s) => `${s.family} (${(s.readouts || []).join(', ')})`).join(', ')}`
    : 'every summary the planner builds';
  return [capabilities.raw_data_retained === false ? 'raw data not retained' : 'raw data retained',
    capabilities.query_time_results_retained ? 'query-time results retained' : 'query-time results not retained', summaries].join(' · ');
}

// The queries of a `PlanningWorkload` JSON, batch then repeating (the
// planner's entry order): each query's text, how often it runs and its
// accuracy target as text.
function planningWorkloadQueries(workload) {
  const qw = (workload && workload.query_workload) || {};
  const accuracy = (requirements) => accuracyText((requirements || {}).accuracy) || 'exact';
  const batch = (qw.query_batch || []).map((entry) => ({
    text: entry.query, runs: entry.invocations > 1 ? `${entry.invocations} times` : 'once', accuracy: accuracy(entry.requirements),
  }));
  const repeating = (qw.repeating_queries || []).map((entry) => {
    const demand = entry.demand || {};
    // `fixed_interval_at` (`interval` and `evaluation_phase`, ms).
    const at = typeof demand === 'object' ? demand.fixed_interval_at : undefined;
    const interval = at ? at.interval : undefined;
    const phase = at && at.evaluation_phase ? ` at phase ${durationText(at.evaluation_phase)}` : '';
    return { text: entry.query, runs: typeof interval === 'number' ? `every ${durationText(interval)}${phase}` : compactWire(demand), accuracy: accuracy(entry.requirements) };
  });
  return batch.concat(repeating);
}

// Whether `value` is a `PlanningWorkload` JSON (to plan) rather than a
// stage document (to show).
function isPlanningWorkload(value) {
  return !!value && typeof value === 'object' && !isStagePipelineDocument(value) && !!value.query_workload && typeof value.query_workload === 'object';
}
