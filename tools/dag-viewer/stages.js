// DOM-free logic of the Stage Viewer: checking an `asap-stage-pipeline/v2`
// document (written by the `asap_planner` devtool), the Stage 3 candidate
// list, and each stage's DAG as cytoscape elements with readable labels.
// app.js draws them; test_viewer.py runs this file headless.
const STAGE_PIPELINE_FORMAT = 'asap-stage-pipeline/v2';

// A stage `run_until` stopped before, or one after a failed stage, is null;
// the viewer shows it empty.
function fillStagesNotRun(doc) {
  if (!doc || typeof doc !== 'object') return doc;
  const empty = {
    stage1_logical_asap_optimization: { combinations: 0, capped: false, candidates: [] },
    stage2_physical_asap_optimization: { candidates: [], physical_candidates: [], pruned_candidates: [] },
    stage3_plan_selection: { selected_dag: null, materialized_views: [], pruned_candidates: [] },
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

// One stage DAG in one shape for both kinds. A logical DAG (Stage 0 and
// Stage 1) is flat: node i is `nodes[i]`, its inputs are ids inside its
// operator, and each query's root is `{"Operator": id}` or
// `{"Scalar": expr}`. A physical DAG (Stage 2) has node ids, an edge list,
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

function uniqueIds(list, where, errors) {
  const ids = new Set();
  list.forEach((entry, index) => {
    if (!entry || typeof entry.id !== 'string' || !entry.id) errors.push(`${where}[${index}]: id must be a non-empty string`);
    else if (ids.has(entry.id)) errors.push(`${where}[${index}]: duplicate id ${entry.id}`);
    else ids.add(entry.id);
  });
  return ids;
}

// The per-candidate selection table the panels read, from Stage 3's
// `Selection` (`stage3_plan_selection`): the selected plan `P1` with its
// cost and accepted accuracy.
function selectionTable(doc) {
  const stage3 = doc.stage3_plan_selection || {};
  const table = {
    selected_candidate: null, cost_per_candidate: {}, accepted_accuracy_per_candidate: {},
    error: stage3.error, warnings: stage3.warnings,
    materialized_views: stage3.materialized_views, materialized_views_error: stage3.materialized_views_error,
    tumbling_window_retention_ms: stage3.tumbling_window_retention_ms, search_report: stage3.search_report,
  };
  if (stage3.cost) {
    table.selected_candidate = 'P1';
    table.cost_per_candidate.P1 = stage3.cost;
    table.accepted_accuracy_per_candidate.P1 = stage3.accepted_accuracy_per_query;
  }
  return table;
}

// The selection table's problems.
function validateSelection(selection, physicalById) {
  const errors = [];
  if (selection.selected_candidate !== null && !physicalById.has(selection.selected_candidate)) {
    errors.push('selected_candidate must name a Stage 2 candidate or be null');
  }
  Object.entries(selection.cost_per_candidate).forEach(([id, cost]) => {
    const at = `cost_per_candidate.${id}`;
    if (!physicalById.has(id)) { errors.push(`${at}: not a Stage 2 candidate`); return; }
    // `cost_per_second` is null under cost model 3 (own cluster), which has no unit costs.
    if (!cost || (cost.cost_per_second !== null && (typeof cost.cost_per_second !== 'number' || !Number.isFinite(cost.cost_per_second)))) errors.push(`${at}: cost_per_second must be a number or null`);
    if (!cost || typeof cost.cost_unit !== 'string') errors.push(`${at}: cost_unit must be a string`);
    const nodeIds = new Set(physicalById.get(id).dag.nodes.map((node) => String(node.id)));
    Object.keys((cost && cost.per_node) || {}).forEach((key) => {
      if (!nodeIds.has(key)) errors.push(`${at}.per_node names missing node ${key}`);
    });
  });
  return errors;
}

// The checks of Stage 2's `physical_candidates` and `pruned_candidates` and
// Stage 3's `pruned_candidates` (the memo view reads them), as messages.
// Physical candidates are numbered by their position; an edge's `producer`
// and a Stage 3 entry's `candidate` and `dominated_by` index them.
function validatePrunedAndPhysical(stage2, stage3) {
  const errors = [];
  const physical = stage2.physical_candidates;
  const pruned2 = stage2.pruned_candidates;
  const pruned3 = stage3.pruned_candidates === undefined ? [] : stage3.pruned_candidates;
  if (!Array.isArray(physical) || !Array.isArray(pruned2)) return ['stage2_physical_asap_optimization: physical_candidates and pruned_candidates must be arrays'];
  if (!Array.isArray(pruned3)) return ['stage3_plan_selection.pruned_candidates must be an array'];
  const isIndex = (v) => Number.isInteger(v) && v >= 0;
  const isCandidate = (v) => isIndex(v) && v < physical.length;
  const hasProperties = (p) => !!p && typeof p.timing === 'string' && p.lifetime !== undefined;
  physical.forEach((c, k) => {
    const at = `stage2_physical_asap_optimization.physical_candidates[${k}]`;
    if (!c || c.id !== k || !isIndex(c.group) || !isIndex(c.index_in_group) || !hasProperties(c.properties) || !Array.isArray(c.inputs)) {
      errors.push(`${at}: needs id ${k}, group, index_in_group, properties {timing, lifetime} and inputs`);
      return;
    }
    c.inputs.forEach((input, i) => {
      if (!input || !isIndex(input.group) || !input.requirement || !Array.isArray(input.edges)
        || !input.edges.every((e) => e && isCandidate(e.producer) && typeof e.selected === 'boolean'
          && (e.rejection === null || typeof e.rejection === 'string'))) {
        errors.push(`${at}.inputs[${i}]: needs group, requirement and edges whose producer is a physical candidate`);
      }
    });
  });
  pruned2.forEach((p, k) => {
    if (!p || !isIndex(p.group) || !isIndex(p.index_in_group) || !hasProperties(p.properties) || typeof p.reason !== 'string') {
      errors.push(`stage2_physical_asap_optimization.pruned_candidates[${k}]: needs group, index_in_group, properties and reason`);
    }
  });
  pruned3.forEach((p, k) => {
    if (!p || !isIndex(p.group) || !isCandidate(p.candidate) || typeof p.reason !== 'string'
      || !(p.dominated_by === null || isCandidate(p.dominated_by))) {
      errors.push(`stage3_plan_selection.pruned_candidates[${k}]: needs group, candidate and dominated_by naming physical candidates, and reason`);
    }
  });
  return errors;
}

// Every shape problem, as readable strings; empty means the document can be
// shown. The viewer refuses a document with problems rather than guessing.
function validateStagePipeline(doc) {
  if (!isStagePipelineDocument(doc)) return [`format must be ${JSON.stringify(STAGE_PIPELINE_FORMAT)}`];
  const errors = [];
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
  if (doc.accuracy_guarantees !== undefined && !ACCURACY_GUARANTEE_MODES.includes(doc.accuracy_guarantees)) {
    errors.push(`accuracy_guarantees must be ${ACCURACY_GUARANTEE_MODES.map((m) => JSON.stringify(m)).join(' or ')}`);
  }
  errors.push(...validateLogicalDag(stage0.dag, 'stage0_query_language_frontends', queries.length));

  const logical = Array.isArray(stage1.candidates) ? stage1.candidates : [];
  if (!Array.isArray(stage1.candidates)) errors.push('stage1_logical_asap_optimization.candidates must be an array');
  uniqueIds(logical, 'stage1_logical_asap_optimization.candidates', errors);
  const composed = new Set();
  logical.forEach((candidate) => {
    if (!candidate) return;
    const where = `stage1_logical_asap_optimization ${candidate.id}`;
    if (candidate.dag) {
      composed.add(candidate.id);
      errors.push(...validateLogicalDag(candidate.dag, where, queries.length));
    } else if (typeof candidate.error !== 'string') {
      errors.push(`${where}: needs a dag or an error`);
    }
  });

  const physical = Array.isArray(stage2.candidates) ? stage2.candidates : [];
  if (!Array.isArray(stage2.candidates)) errors.push('stage2_physical_asap_optimization.candidates must be an array');
  uniqueIds(physical, 'stage2_physical_asap_optimization.candidates', errors);
  const physicalById = new Map();
  physical.forEach((candidate) => {
    if (!candidate) return;
    physicalById.set(candidate.id, candidate);
    if (!composed.has(candidate.from_logical)) {
      errors.push(`stage2_physical_asap_optimization ${candidate.id}: from_logical ${JSON.stringify(candidate.from_logical)} is not a composed Stage 1 candidate`);
    }
    errors.push(...validatePhysicalDag(candidate.dag, `stage2_physical_asap_optimization ${candidate.id}`, queries.length));
  });
  errors.push(...validatePrunedAndPhysical(stage2, stage3));
  errors.push(...validateDataWorkload(doc.workload.data_workload));
  if (errors.length) return errors;
  const table = selectionTable(doc);
  const selectionErrors = validateSelection(table, physicalById);
  if (selectionErrors.length) return selectionErrors;
  return validateMaterializedViews(table, physicalById);
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

// What Stage 3 accepted for a valid candidate's queries
// (`accepted_accuracy_per_candidate[id]`:
// one entry per query), as "q1: …; q2: …"; null
// when the document has none.
function acceptedText(queries) {
  if (!Array.isArray(queries)) return null;
  const parts = queries.map((q, index) => (q && q.summary ? `q${index + 1}: ${q.summary}` : null)).filter(Boolean);
  return parts.length ? parts.join('; ') : null;
}

// Stage 3's view of every candidate the document shows: the selected one
// ("not judged" for another, as when selection failed with an error), then
// the Stage 1 ones that did not compose ("composition error") with their
// error. `logical` names the Stage 1 candidate to show beside the row.
function stageCandidateRows(doc) {
  const selection = selectionTable(doc);
  const costs = selection.cost_per_candidate;
  const accepted = selection.accepted_accuracy_per_candidate;
  const rows = ((doc.stage2_physical_asap_optimization || {}).candidates || []).map((candidate) => {
    const cost = costs[candidate.id];
    const selected = candidate.id === selection.selected_candidate;
    return {
      id: candidate.id,
      logical: candidate.from_logical,
      display_name: candidate.display_name,
      total: cost ? cost.cost_per_second : null,
      status: selected ? 'selected' : 'not judged',
      reason: null,
      accepted: acceptedText(accepted[candidate.id]),
    };
  });
  ((doc.stage1_logical_asap_optimization || {}).candidates || []).filter((c) => 'error' in c).forEach((c) => rows.push({
    id: c.id, logical: c.id, display_name: c.display_name, total: null, status: 'composition error', reason: c.error,
  }));
  const order = { selected: 0, 'not judged': 1, 'composition error': 2 };
  return rows.sort((a, b) => order[a.status] - order[b.status]);
}

// The candidate list's header: how many candidates Stage 1 enumerated, how
// many the document shows when it caps them, why Stage 1 failed (a Pass 1
// error) and why plan selection failed when it failed for a reason other
// than rejecting every candidate, then the selection's warnings (e.g. a
// very large peak memory with no declared memory capacity).
// Plan ids in plan-number order: P2 before P2-m1 before P10.
function byPlanNumber(a, b) {
  return a.localeCompare(b, undefined, { numeric: true });
}

function stageCandidateHeader(doc) {
  const logical = doc.stage1_logical_asap_optimization || {};
  const selection = selectionTable(doc);
  const shown = logical.capped ? ` (document shows ${(logical.candidates || []).length})` : '';
  const error = [
    logical.error ? ` · Stage 1 failed: ${logical.error}` : '',
    selection.error ? ` · selection failed: ${selection.error}` : '',
    ...(selection.warnings || []).map((warning) => ` · warning: ${warning}`),
  ].join('');
  return `${logical.combinations ?? 0} candidates${shown}${error}`;
}

// Whether Stage 3 checked accuracy guarantees (`accuracy_guarantees`), for
// the candidate list's header; "" for a document that does not say.
const ACCURACY_GUARANTEE_MODES = ['ignored', 'enforced'];
function accuracyGuaranteesText(doc) {
  return {
    ignored: 'accuracy guarantees ignored: every output accepted, candidates compared on cost alone',
    enforced: 'accuracy guarantees enforced',
  }[doc.accuracy_guarantees] || '';
}

// The search behind the document and Stage 3's search report, for the
// candidate list's header, e.g. "candidate-connection search · search
// optimal · MILP 936 variables, …". "" for a document that names no search.
function searchText(doc) {
  const name = { 'candidate-connections': 'candidate-connection search' }[doc.search];
  if (!name) return '';
  const parts = [name];
  const report = (doc.stage3_plan_selection || {}).search_report;
  if (report) {
    parts.push(`search ${String(report.status).replace(/_/g, ' ')}`);
    if (typeof report.cost_lower_bound_per_second === 'number') parts.push(`cost lower bound ${Number(report.cost_lower_bound_per_second.toPrecision(4))}`);
    parts.push(`MILP ${report.milp_variables} variables, ${report.milp_constraints} constraints · model ${report.model_construction_ms} ms, solve ${report.solver_ms} ms, validation ${report.validation_ms} ms`);
  }
  return parts.join(' · ');
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

// `{"Variant": {"width": 272, ...}}` as "width 272 · ..."; `heap_size` reads as "heap".
function wireParams(params) {
  const inner = params && typeof params === 'object' ? Object.values(params)[0] : null;
  if (!inner || typeof inner !== 'object') return [];
  return Object.entries(inner).map(([key, value]) => `${key.replace(/_size$/, '')} ${compactWire(value)}`);
}

// A summary family with its configured parameters, e.g. "CmsWithHeap · depth 7 · heap 100 · width 272".
function summaryFamilyText(family) {
  if (family && Array.isArray(family.Sketch)) {
    const kind = family.Sketch[0] || {};
    return [kind.algorithm || 'sketch'].concat(wireParams(kind.params)).join(' · ');
  }
  if (family && Array.isArray(family.ExactAggregate)) return `exact ${family.ExactAggregate[0]}`;
  return compactWire(family);
}

// Whether a summary keeps one state per group or one shared (Hydra) state for all groups.
function summaryInstancesText(grouping, reduction) {
  const shared = grouping && grouping.SharedMultiSubpopulation;
  if (shared) return [`one shared ${shared.kind}`].concat(wireParams(shared.params)).join(' · ');
  if (grouping === 'PerSubpopulationInstance') return reduction === 'PerEntity' ? 'one per series' : 'one per group';
  return compactWire(grouping);
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
      lines.push(`summary: ${summaryFamilyText(op.family)}`);
      if (grouping) lines.push(grouping);
      lines.push(`instances: ${summaryInstancesText(op.grouping, op.reduction)}`);
      break;
    case 'SummaryEstimate': lines.push(`query: ${compactWire(op.query)}`); break;
    case 'SummaryMerge':
      if (op.newest_tumbling_windows) lines.push(`newest ${op.newest_tumbling_windows} tumbling windows`);
      break;
    case 'SummaryTumblingWindowChain':
      lines.push(`summary: ${summaryFamilyText(op.family)}`);
      lines.push(`${op.tumbling_window_count} tumbling windows × ${durationText(op.tumbling_window_width_ms)}`);
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

// The time range a tumbling window covers, relative to the evaluation time `t`: tumbling window `k` (0 is the
// newest) of width `width` covers "t−(k+1)·width … t−k·width".
function tumblingWindowTimeText(k, width) {
  const end = k === 0 ? 't' : `t−${durationText(k * width)}`;
  return `t−${durationText((k + 1) * width)} … ${end}`;
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
// "shared ×n" in its box when n > 1). Options: `physical` (a Stage 2 DAG:
// nodes show their timing), `costPerNode` (the candidate's Stage 3
// `per_node` costs), `queryIds` (the workload's query ids, to mark each
// query's root), `expandedChains` (ids of SummaryTumblingWindowChain nodes to draw as their
// tumbling windows: per tumbling window `k`, newest first, a TimeShift by k tumbling windows (`n<id>p<k>s`,
// none for the newest), a TimeRange of one tumbling window (`n<id>p<k>r`) and the
// tumbling window's build (`n<id>p<k>`), each tumbling window fed by the chain's input and read by
// the chain's readers). Display only: the document keeps the chain.
function stageLaneElements(dag, options) {
  const { physical = false, costPerNode = null, queryIds = [], expandedChains = [] } = options || {};
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
  const reachedBy = (id) => (reach.get(id) || []).map(queryName);
  const expanded = new Set(lane.nodes.filter((node) => node.kind === 'SummaryTumblingWindowChain' && expandedChains.includes(node.id)).map((node) => node.id));

  const nodes = [];
  lane.nodes.forEach((node) => {
    const input = inputOf.get(node.id);
    const operator = physical ? node.raw.payload : node.raw.operator;
    const queries = reachedBy(node.id);
    if (expanded.has(node.id)) {
      const body = node.body;
      const width = body.tumbling_window_width_ms;
      for (let k = 0; k < body.tumbling_window_count; k += 1) {
        const id = `n${node.id}p${k}`;
        const common = { tumblingWindowOf: node.id, tumblingWindowIndex: k, reachedBy: queries, rootFor: [], timing: node.timing, node: node.raw };
        if (k > 0) {
          nodes.push({ data: Object.assign({ id: `${id}s`, nodeId: `${node.id}p${k}s`, kind: 'TimeShift', group: group('TimeShift'),
            label: `TimeShift\nshift: ${durationText(k * width)}` }, common), classes: `tumblingWindow ${timingClass(node.timing)}`.trim() });
        }
        nodes.push({ data: Object.assign({ id: `${id}r`, nodeId: `${node.id}p${k}r`, kind: 'TimeRange', group: group('TimeRange'),
          label: `TimeRange\nrange: ${durationText(width)}` }, common), classes: `tumblingWindow ${timingClass(node.timing)}`.trim() });
        nodes.push({ data: Object.assign({ id, nodeId: `${node.id}p${k}`, kind: 'tumblingWindow', group: 'summary',
          label: [`tumbling window ${k + 1} of ${body.tumbling_window_count}`, `summary: ${summaryFamilyText(body.family)}`, `window ${tumblingWindowTimeText(k, width)}`,
            'click to collapse'].join('\n') }, common), classes: `tumblingWindow ${timingClass(node.timing)}`.trim() });
      }
      return;
    }
    const lines = stageNodeLines(operator, input && input.schema);
    const cost = costPerNode ? costPerNode[String(node.id)] : undefined;
    if (physical && node.timing !== undefined) lines.push(`⏱ ${formatTiming(node.timing)}`);
    if (cost && typeof cost.cost_per_second === 'number') {
      lines.push(`cost ${Number(cost.cost_per_second.toPrecision(3))}${cost.multiplier === 1 || cost.multiplier === undefined ? '' : ` × ${cost.multiplier}`}`);
    }
    if (queries.length > 1) lines.push(`shared ×${queries.length}`);
    const roots = rootFor.get(node.id) || [];
    if (roots.length) lines.push(`root of ${roots.join(', ')}`);
    if (node.kind === 'SummaryTumblingWindowChain') lines.push('click to expand');
    nodes.push({
      data: {
        id: `n${node.id}`, nodeId: node.id, kind: node.kind, group: group(node.kind), label: lines.join('\n'),
        rootFor: roots, reachedBy: queries, timing: node.timing, cost, node: node.raw, expandable: node.kind === 'SummaryTumblingWindowChain',
      },
      classes: timingClass(node.timing),
    });
  });
  const edges = [];
  const edge = (id, source, target, raw) => edges.push({ data: { id, source, target, edge: raw } });
  lane.edges.forEach((e, index) => {
    const into = expanded.has(e.consumer);
    const from = expanded.has(e.producer);
    if (!into && !from) { edge(`e${index}`, `n${e.producer}`, `n${e.consumer}`, e.raw); return; }
    // An expanded chain's input feeds each tumbling window's TimeRange; each tumbling window feeds
    // the chain's readers.
    const tumblingWindowCount = (chain) => byId.get(chain).body.tumbling_window_count;
    if (into) {
      for (let k = 0; k < tumblingWindowCount(e.consumer); k += 1) {
        const tumblingWindowId = `n${e.consumer}p${k}`;
        edge(`e${index}p${k}`, `n${e.producer}`, k > 0 ? `${tumblingWindowId}s` : `${tumblingWindowId}r`, e.raw);
        if (k > 0) edge(`e${index}p${k}s`, `${tumblingWindowId}s`, `${tumblingWindowId}r`, null);
        edge(`e${index}p${k}r`, `${tumblingWindowId}r`, tumblingWindowId, null);
      }
    }
    if (from) {
      // A merge of the chain's newest tumbling windows reads only those.
      const consumer = byId.get(e.consumer);
      const read = (consumer && consumer.body && consumer.body.newest_tumbling_windows) || tumblingWindowCount(e.producer);
      for (let k = 0; k < read; k += 1) {
        edge(`e${index}q${k}`, `n${e.producer}p${k}`, into ? `n${e.consumer}p0r` : `n${e.consumer}`, e.raw);
      }
    }
  });
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
      data: { id: `${id}e${k}`, source: expanded.has(producer) ? `n${producer}p0` : `n${producer}`, target: id, edge: null },
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

const VIEW_REFRESHES = ['at_ingestion', 'on_first_read'];

// The selected plan's materialized views (`stage3_plan_selection.materialized_views`,
// from the installable plan): each names its installed node, how it is
// refreshed, its coverage's definition and its retention, and the node of the
// selected Stage 2 candidate it is installed from (or null). Absent in older
// documents; empty without a selection.
function validateMaterializedViews(selection, physicalById) {
  const views = selection.materialized_views;
  if (views === undefined) return [];
  const at = 'stage3_plan_selection.materialized_views';
  if (!Array.isArray(views)) return [`${at} must be an array`];
  if (selection.selected_candidate === null && views.length) return [`${at} must be empty without a selected plan`];
  const selected = selection.selected_candidate === null ? null : physicalById.get(selection.selected_candidate);
  const nodeIds = new Set(selected ? selected.dag.nodes.map((node) => node.id) : []);
  const errors = [];
  views.forEach((output, index) => {
    const where = `${at}[${index}]`;
    if (!output || typeof output !== 'object') { errors.push(`${where} must be an object`); return; }
    if (typeof output.node_id !== 'number') errors.push(`${where}: node_id must be a number`);
    if (!VIEW_REFRESHES.includes(output.refresh)) errors.push(`${where}: refresh must be one of ${VIEW_REFRESHES.join(', ')}`);
    if (!output.coverage || typeof output.coverage.definition_id !== 'string') errors.push(`${where}: coverage.definition_id must be a string`);
    if (!output.inputs || !Array.isArray(output.inputs.raw_data_windows) || !Array.isArray(output.inputs.materialized_views)) {
      errors.push(`${where}: inputs needs raw_data_windows and materialized_views arrays`);
    }
    if (!(typeof output.retention_ms === 'number' && output.retention_ms >= 0)) errors.push(`${where}: retention_ms must be a non-negative number`);
    if (output.stage2_node_id !== null && output.stage2_node_id !== undefined && !nodeIds.has(output.stage2_node_id)) {
      errors.push(`${where}: stage2_node_id ${output.stage2_node_id} is not a node of the selected candidate ${selection.selected_candidate}`);
    }
  });
  return errors;
}

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
    nonNegative: facts.values_non_negative ? 'yes' : String(facts.metric_type).toLowerCase() === 'counter' ? 'yes (counter)' : '—',
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
// (Stage 2): the queries that read it, directly or through other nodes.
function physicalReaders(dag, nodeId, queryIds) {
  return (stageQueryReach(stageDag(dag, true)).get(nodeId) || []).map((index) => queryIds[index] || `query #${index + 1}`);
}

// The selected plan's materialized views as table rows: the coverage's
// definition, the Stage 2 node's operator and summary parameters, its tumbling window
// (a tumbling-window chain's tumbling windows, newest first), how and when it is refreshed, its
// retention, the Stage 3 cost of its Stage 2 node and the queries that read
// it. Empty without a selection.
function materializedViewRows(doc) {
  const selection = selectionTable(doc);
  const views = Array.isArray(selection.materialized_views) ? selection.materialized_views : [];
  const candidate = ((doc.stage2_physical_asap_optimization || {}).candidates || []).find((c) => c.id === selection.selected_candidate);
  if (!candidate) return [];
  const queryIds = doc.workload.queries.map((q) => q.id);
  const nodes = new Map(candidate.dag.nodes.map((node) => [node.id, node]));
  const perNode = ((selection.cost_per_candidate || {})[selection.selected_candidate] || {}).per_node || {};
  return views.map((output) => {
    const node = nodes.get(output.stage2_node_id);
    const [kind, body] = node ? stageOperatorParts(node.payload) : ['—', {}];
    const summary = kind === 'SummaryAgg' || kind === 'SummaryTumblingWindowChain' ? summaryFamilyText(body.family) : '';
    const tumblingWindow = kind === 'SummaryTumblingWindowChain' && typeof output.installed_position === 'number'
      ? `tumbling window ${output.installed_position + 1} of ${output.installed_count} (newest first), ${durationText(body.tumbling_window_width_ms)} wide` : '—';
    const cost = perNode[String(output.stage2_node_id)];
    return {
      nodeId: output.node_id,
      stage2NodeId: node ? output.stage2_node_id : null,
      definition: output.coverage.definition_id,
      kind: summary ? `${kind} · ${summary}` : kind,
      tumblingWindow,
      refresh: output.refresh === 'at_ingestion' ? 'at ingestion' : 'on first read (materialized across evaluations)',
      inputs: `${output.inputs.raw_data_windows.length} raw-data window(s)` +
        (output.inputs.materialized_views.length ? `, views ${output.inputs.materialized_views.join(', ')}` : ''),
      retention: durationText(output.retention_ms),
      window: output.inputs.raw_data_windows.map((w) => `t−${durationText(w.end_offset_ms + w.width_ms)} … ${w.end_offset_ms ? `t−${durationText(w.end_offset_ms)}` : 't'}`).join(', ') || '—',
      position: output.installed_position,
      cost: cost ? cost.detail || '' : '',
      readBy: node ? physicalReaders(candidate.dag, output.stage2_node_id, queryIds) : [],
    };
  });
}

// The selected plan's materialized views grouped by the Stage 2 node they are
// installed from: one group per tumbling-window chain (its tumbling windows, newest first, with
// "count × width"), one per other node. Each group has its Stage 2 node,
// definition(s), kind, how it is refreshed, its retention (a range when the
// tumbling windows differ), the Stage 3 cost and the queries that read it.
function materializedViewGroups(doc) {
  const rows = materializedViewRows(doc);
  const candidate = ((doc.stage2_physical_asap_optimization || {}).candidates || []).find((c) => c.id === selectionTable(doc).selected_candidate);
  const nodes = new Map(candidate ? candidate.dag.nodes.map((node) => [node.id, node]) : []);
  const groups = [];
  const byNode = new Map();
  rows.forEach((row) => {
    const key = row.stage2NodeId === null ? `installed ${row.nodeId}` : row.stage2NodeId;
    if (!byNode.has(key)) { byNode.set(key, []); groups.push(key); }
    byNode.get(key).push(row);
  });
  const joined = (values) => [...new Set(values)].join(', ');
  return groups.map((key) => {
    const tumblingWindows = byNode.get(key).slice().sort((a, b) => (a.position ?? 0) - (b.position ?? 0));
    const first = tumblingWindows[0];
    const node = nodes.get(first.stage2NodeId);
    const [kind, body] = node ? stageOperatorParts(node.payload) : ['—', {}];
    const retention = tumblingWindows.map((p) => p.retention);
    return {
      stage2NodeId: first.stage2NodeId,
      nodeIds: tumblingWindows.map((p) => p.nodeId),
      definition: joined(tumblingWindows.map((p) => p.definition)),
      kind: first.kind,
      tumblingWindowsText: kind === 'SummaryTumblingWindowChain' ? `${body.tumbling_window_count} × ${durationText(body.tumbling_window_width_ms)}` : '—',
      refresh: joined(tumblingWindows.map((p) => p.refresh)),
      retention: new Set(retention).size === 1 ? retention[0] : `${retention[0]} … ${retention[retention.length - 1]}`,
      cost: first.cost,
      readBy: first.readBy,
      tumblingWindows,
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
    const interval = body && typeof body === 'object' ? body.fixed_interval : undefined;
    parts.push(typeof interval === 'number' ? `every ${durationText(interval)}` : `repeated: ${compactWire(body)}`);
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
    const interval = typeof demand === 'object' ? demand.fixed_interval : undefined;
    return { text: entry.query, runs: typeof interval === 'number' ? `every ${durationText(interval)}` : compactWire(demand), accuracy: accuracy(entry.requirements) };
  });
  return batch.concat(repeating);
}

// Whether `value` is a `PlanningWorkload` JSON (to plan) rather than a
// stage document (to show).
function isPlanningWorkload(value) {
  return !!value && typeof value === 'object' && !isStagePipelineDocument(value) && !!value.query_workload && typeof value.query_workload === 'object';
}
