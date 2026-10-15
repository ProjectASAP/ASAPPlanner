// DOM-free logic of the Stage Viewer: checking an `asap-stage-pipeline/v1`
// document (written by the `stage_pipeline` devtool), the Stage 3 candidate
// list, and each stage's DAG as cytoscape elements with readable labels.
// app.js draws them; test_viewer.py runs this file headless.
const STAGE_PIPELINE_FORMAT = 'asap-stage-pipeline/v1';

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
  Object.entries(body).forEach(([field, value]) => { if (!STAGE_CHILD_FIELDS.includes(field)) stageScalarRefs(value, ids); });
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
    case 'Arithmetic': return `(${stageScalarText(body.left)} ${Object.keys(body.op)[0] || body.op} ${stageScalarText(body.right)})`;
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
        materialization: node.materialization, timing: node.output_state ? node.output_state.timing : undefined, raw: node,
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
      id, kind, body, schema: node.schema, guarantee: node.guarantee,
      materialization: node.materialization, timing: node.timing === null ? undefined : node.timing, raw: node,
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

function validateSelection(selection, physicalById) {
  const errors = [];
  if (!selection || typeof selection !== 'object') return ['stage3_plan_selection must be an object'];
  if (selection.selected !== null && !physicalById.has(selection.selected)) {
    errors.push('stage3_plan_selection.selected must name a Stage 2 candidate or be null');
  }
  if (!Array.isArray(selection.rejected)) return errors.concat('stage3_plan_selection.rejected must be an array');
  const accounted = new Set(selection.selected === null ? [] : [selection.selected]);
  selection.rejected.forEach((entry, index) => {
    const at = `stage3_plan_selection.rejected[${index}]`;
    const id = entry && entry.id;
    if (!physicalById.has(id)) errors.push(`${at}: ${JSON.stringify(id)} is not a Stage 2 candidate`);
    else if (accounted.has(id)) errors.push(`${at}: ${id} is already selected or rejected`);
    if (!entry || typeof entry.valid !== 'boolean') errors.push(`${at}: valid must be true or false`);
    accounted.add(id);
  });
  if (selection.selected !== null) {
    physicalById.forEach((_, id) => { if (!accounted.has(id)) errors.push(`stage3_plan_selection: ${id} is neither selected nor rejected`); });
  }
  const costs = selection.costs;
  if (!costs || typeof costs !== 'object' || Array.isArray(costs)) return errors.concat('stage3_plan_selection.costs must be an object');
  Object.entries(costs).forEach(([id, cost]) => {
    const at = `stage3_plan_selection.costs.${id}`;
    if (!physicalById.has(id)) { errors.push(`${at}: not a Stage 2 candidate`); return; }
    if (!cost || typeof cost.total !== 'number' || !Number.isFinite(cost.total)) errors.push(`${at}: total must be a number`);
    if (!cost || typeof cost.unit !== 'string') errors.push(`${at}: unit must be a string`);
    const nodeIds = new Set(physicalById.get(id).dag.nodes.map((node) => String(node.id)));
    Object.keys((cost && cost.per_node) || {}).forEach((key) => {
      if (!nodeIds.has(key)) errors.push(`${at}.per_node names missing node ${key}`);
    });
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
  errors.push(...validateLogicalDag(stage0.dag, 'stage0_query_language_frontends', queries.length));

  const logical = Array.isArray(stage1.candidates) ? stage1.candidates : [];
  if (!Array.isArray(stage1.candidates)) errors.push('stage1_logical_asap_optimization.candidates must be an array');
  const logicalIds = uniqueIds(logical, 'stage1_logical_asap_optimization.candidates', errors);
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
  (Array.isArray(stage2.pruned) ? stage2.pruned : []).forEach((entry, index) => {
    if (!entry || !logicalIds.has(entry.from_logical)) errors.push(`stage2_physical_asap_optimization.pruned[${index}]: from_logical is not a Stage 1 candidate`);
  });
  if (errors.length) return errors;
  return validateSelection(stage3, physicalById);
}

// A query's accuracy target (`AccuracyTarget` JSON: `accuracy_metric`,
// `error`, `failure_probability`, `accuracy_scope`) as one line, e.g. "ε=0.01, δ=0.001
// (rank)" or "ε=0.01 (metric not set; default relative)"; "exact" for
// zero error with certainty; "" for a malformed target. `defaultMetric` is the
// query's default metric the document shows for a target without one.
function accuracyText(target, defaultMetric) {
  if (!target || typeof target !== 'object') return '';
  if (target.error === 0 && target.failure_probability === 0) return 'exact';
  const delta = target.failure_probability !== undefined && target.failure_probability !== null
    ? `, δ=${target.failure_probability}` : '';
  const metric = target.accuracy_metric || `metric not set${defaultMetric ? `; default ${defaultMetric}` : ''}`;
  return `ε=${target.error}${delta} (${metric})`;
}

// What Stage 3 accepted for a valid candidate's queries (`accepted[id]`:
// one entry per query), as "q1: …; q2: …"; null
// when the document has none.
function acceptedText(queries) {
  if (!Array.isArray(queries)) return null;
  const parts = queries.map((q, index) => (q && q.summary ? `q${index + 1}: ${q.summary}` : null)).filter(Boolean);
  return parts.length ? parts.join('; ') : null;
}

// Stage 3's view of every candidate the document shows: the selected one,
// the other valid ones cheapest first ("costlier"), the rejected ones with
// their reasons ("not judged" when selection failed with an error), then
// the Stage 2 pruned ones and the Stage 1 ones that did not compose
// ("composition error") with theirs. `logical` names the Stage 1
// candidate to show beside the row; `rank` counts the priced ones.
function stageCandidateRows(doc) {
  const selection = doc.stage3_plan_selection || {};
  const costs = selection.costs || {};
  const rejected = new Map((selection.rejected || []).map((r) => [r.id, r]));
  const accepted = selection.accepted || {};
  const physical = doc.stage2_physical_asap_optimization || {};
  const rows = (physical.candidates || []).map((candidate) => {
    const cost = costs[candidate.id];
    const rejection = rejected.get(candidate.id);
    // A failed selection (`error`) judged no candidate.
    const status = candidate.id === selection.selected ? 'selected'
      : cost ? 'costlier' : rejection || !selection.error ? 'rejected' : 'not judged';
    return {
      id: candidate.id,
      logical: candidate.from_logical,
      display_name: candidate.display_name,
      total: cost ? cost.total : null,
      unit: cost ? cost.unit : null,
      rank: null,
      status,
      reason: status === 'rejected' && rejection ? rejection.reason : null,
      accepted: acceptedText(accepted[candidate.id]),
    };
  });
  (physical.pruned || []).forEach((p) => rows.push({
    id: p.from_logical, logical: p.from_logical, display_name: p.display_name, total: null, unit: null, rank: null, status: 'pruned', reason: p.reason,
  }));
  ((doc.stage1_logical_asap_optimization || {}).candidates || []).filter((c) => 'error' in c).forEach((c) => rows.push({
    id: c.id, logical: c.id, display_name: c.display_name, total: null, unit: null, rank: null, status: 'composition error', reason: c.error,
  }));
  const order = { selected: 0, costlier: 1, rejected: 2, 'not judged': 2, pruned: 3, 'composition error': 4 };
  rows.sort((a, b) => order[a.status] - order[b.status] || (a.total ?? 0) - (b.total ?? 0));
  let rank = 0;
  rows.forEach((row) => { if (row.total !== null) row.rank = ++rank; });
  return rows;
}

// The candidate list's header: how many candidates Stage 1 enumerated, how
// many the document shows when it caps them, why Stage 1 failed (a Pass 1
// error) and why plan selection failed when it failed for a reason other
// than rejecting every candidate.
function stageCandidateHeader(doc) {
  const logical = doc.stage1_logical_asap_optimization || {};
  const selection = doc.stage3_plan_selection || {};
  const shown = logical.capped ? ` (document shows ${(logical.candidates || []).length})` : '';
  const error = [
    logical.error ? ` · Stage 1 failed: ${logical.error}` : '',
    selection.error ? ` · selection failed: ${selection.error}` : '',
  ].join('');
  return `${logical.combinations ?? 0} candidates${shown}${error}`;
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
    default: break;
  }
  return lines.slice(0, 4);
}

// A stage DAG (`dag`, as the document writes it) as cytoscape elements:
// one node per operator (`n<id>`), one `Scalar` node per standalone scalar
// query (`s<query position>`) over the nodes it reads, and one edge per
// input. Options: `physical` (a Stage 2 DAG: nodes show their timing),
// `costPerNode` (the candidate's Stage 3 `per_node` costs), `queryIds` (the
// workload's query ids, to mark each query's root).
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

  const nodes = lane.nodes.map((node) => {
    const input = inputOf.get(node.id);
    const operator = physical ? node.raw.payload : node.raw.operator;
    const lines = stageNodeLines(operator, input && input.schema);
    const cost = costPerNode ? costPerNode[String(node.id)] : undefined;
    if (physical && node.timing !== undefined) lines.push(`⏱ ${formatTiming(node.timing)}`);
    if (cost && typeof cost.cost === 'number') {
      lines.push(`cost ${Number(cost.cost.toPrecision(3))}${cost.multiplier === 1 || cost.multiplier === undefined ? '' : ` × ${cost.multiplier}`}`);
    }
    const roots = rootFor.get(node.id) || [];
    if (roots.length) lines.push(`root of ${roots.join(', ')}`);
    return {
      data: {
        id: `n${node.id}`, nodeId: node.id, kind: node.kind, group: group(node.kind), label: lines.join('\n'),
        rootFor: roots, timing: node.timing, cost, node: node.raw,
      },
      classes: node.timing !== undefined && formatTiming(node.timing) === 'ingestion time' ? 'ingestionTime' : '',
    };
  });
  const edges = lane.edges.map((edge, index) => ({
    data: { id: `e${index}`, source: `n${edge.producer}`, target: `n${edge.consumer}`, edge: edge.raw },
  }));
  lane.queries.forEach((query, index) => {
    if (!('scalar' in query)) return;
    const id = `s${index}`;
    const queryId = queryIds[index] || `query #${index + 1}`;
    nodes.push({
      data: {
        id, nodeId: id, kind: 'Scalar', group: group('Scalar'), rootFor: [queryId], node: { Scalar: query.scalar },
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
