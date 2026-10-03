// DOM-free logic for the Stages view: validating an `asap-stage-pipeline/v1`
// document, turning its LogicalASAPDAG / PhysicalASAPDAG lanes into
// cytoscape elements, and ranking physical candidates by cost. Kept apart
// from viewer.js so test_render.py can run it headless.
const STAGE_PIPELINE_FORMAT = 'asap-stage-pipeline/v1';

function isStagePipelineDocument(doc) {
  return !!doc && typeof doc === 'object' && doc.format === STAGE_PIPELINE_FORMAT;
}

// A DAG has one root per batch query, in workload order (`roots`); a
// single `root` is accepted for documents written before that. Logical
// roots are `{"Operator": id}` or `{"Scalar": expr}`; physical roots are
// bare node ids.
function stageDagRoots(dag) {
  if (Array.isArray(dag.roots)) return dag.roots;
  return dag.root === undefined ? [] : [dag.root];
}

// The operator node a root names, or null for a scalar root (it has no
// single operator node to mark).
function stageRootNodeId(root) {
  if (typeof root === 'number') return root;
  if (root && typeof root === 'object' && typeof root.Operator === 'number') return root.Operator;
  return null;
}

function validateStageDag(dag, where, physical, queryCount) {
  const errors = [];
  if (!dag || typeof dag !== 'object') return [`${where}: dag is missing`];
  if (!Array.isArray(dag.nodes) || dag.nodes.length === 0) errors.push(`${where}: dag.nodes must be a non-empty array`);
  if (!Array.isArray(dag.edges)) errors.push(`${where}: dag.edges must be an array`);
  if (errors.length) return errors;
  const ids = new Set();
  dag.nodes.forEach((node, index) => {
    if (!node || typeof node.id !== 'number') { errors.push(`${where}: node #${index} has no numeric id`); return; }
    if (ids.has(node.id)) errors.push(`${where}: duplicate node id ${node.id}`);
    ids.add(node.id);
    if (!node.payload || typeof node.payload.kind !== 'string') errors.push(`${where}: node ${node.id} has no payload.kind`);
    if (physical && !(node.output_state && node.output_state.timing)) errors.push(`${where}: node ${node.id} has no output_state.timing`);
  });
  dag.edges.forEach((edge, index) => {
    if (!edge || !ids.has(edge.producer) || !ids.has(edge.consumer)) errors.push(`${where}: edge #${index} names a missing producer/consumer`);
    else if (physical && !edge.data_state) errors.push(`${where}: edge ${edge.producer}->${edge.consumer} has no data_state`);
  });
  if (dag.roots !== undefined && !Array.isArray(dag.roots)) errors.push(`${where}: roots must be an array`);
  const roots = stageDagRoots(dag);
  if (roots.length === 0) errors.push(`${where}: no roots`);
  if (queryCount && Array.isArray(dag.roots) && roots.length !== queryCount) {
    errors.push(`${where}: ${roots.length} roots for ${queryCount} workload queries`);
  }
  roots.forEach((root, index) => {
    const isLogical = root && typeof root === 'object' && ('Operator' in root || 'Scalar' in root);
    if (physical ? typeof root !== 'number' : !isLogical) errors.push(`${where}: root #${index} is malformed`);
    const id = stageRootNodeId(root);
    if (id !== null && !ids.has(id)) errors.push(`${where}: root ${id} does not name a node`);
  });
  return errors;
}

function validateCandidates(list, where, physical, logicalIds, queryCount) {
  const errors = [];
  if (!Array.isArray(list) || list.length === 0) return [`${where}.candidates must be a non-empty array`];
  const ids = new Set();
  list.forEach((candidate, index) => {
    const at = `${where}.candidates[${index}]`;
    if (!candidate || typeof candidate.id !== 'string' || !candidate.id) { errors.push(`${at}: id must be a non-empty string`); return; }
    if (ids.has(candidate.id)) errors.push(`${at}: duplicate candidate id ${candidate.id}`);
    ids.add(candidate.id);
    errors.push(...validateStageDag(candidate.dag, `${where} ${candidate.id}`, physical, queryCount));
    if (physical && !logicalIds.has(candidate.from_logical)) errors.push(`${at}: from_logical ${JSON.stringify(candidate.from_logical)} is not a stage-1 candidate`);
  });
  return errors;
}

function validateSelection(selection, physicalCandidates) {
  const errors = [];
  const byId = new Map(physicalCandidates.map((candidate) => [candidate && candidate.id, candidate]));
  if (!byId.has(selection.selected)) errors.push('stage3_selection.selected must name a stage-2 candidate');
  const accounted = new Set([selection.selected]);
  if (!Array.isArray(selection.rejected)) errors.push('stage3_selection.rejected must be an array');
  (Array.isArray(selection.rejected) ? selection.rejected : []).forEach((entry, index) => {
    const at = `stage3_selection.rejected[${index}]`;
    const id = entry && entry.id;
    if (!byId.has(id)) errors.push(`${at}: ${JSON.stringify(id)} is not a stage-2 candidate`);
    else if (accounted.has(id)) errors.push(`${at}: ${id} is already selected or rejected`);
    if (!entry || typeof entry.valid !== 'boolean') errors.push(`${at}: valid must be true or false`);
    accounted.add(id);
  });
  byId.forEach((_, id) => { if (!accounted.has(id)) errors.push(`stage3_selection: ${id} is neither selected nor rejected`); });
  const costs = selection.costs === undefined ? {} : selection.costs;
  if (!costs || typeof costs !== 'object' || Array.isArray(costs)) return errors.concat('stage3_selection.costs must be an object');
  Object.entries(costs).forEach(([id, cost]) => {
    const at = `stage3_selection.costs.${id}`;
    if (!byId.has(id)) { errors.push(`${at}: not a stage-2 candidate`); return; }
    if (!cost || typeof cost.total !== 'number' || !Number.isFinite(cost.total)) errors.push(`${at}: total must be a number`);
    if (!cost || typeof cost.unit !== 'string') errors.push(`${at}: unit must be a string`);
    const nodeIds = new Set(((byId.get(id).dag || {}).nodes || []).map((node) => String(node.id)));
    Object.keys((cost && cost.per_node) || {}).forEach((key) => {
      if (!nodeIds.has(key)) errors.push(`${at}.per_node names missing node ${key}`);
    });
  });
  return errors;
}

// Every shape problem, as readable strings; empty means the document can be
// rendered. The viewer refuses a document with errors rather than guessing.
// Later stages may be absent (a partial run), but never without the
// earlier stages they refer to.
function validateStagePipeline(doc) {
  if (!isStagePipelineDocument(doc)) return [`format must be ${JSON.stringify(STAGE_PIPELINE_FORMAT)}`];
  const errors = [];
  const queries = doc.workload && doc.workload.queries;
  if (!Array.isArray(queries) || queries.length === 0) errors.push('workload.queries must be a non-empty array');
  const queryCount = Array.isArray(queries) ? queries.length : 0;
  errors.push(...validateStageDag(doc.stage0_logical && doc.stage0_logical.dag, 'stage0_logical', false, queryCount));
  const stage1 = doc.stage1_logical_asap;
  const stage2 = doc.stage2_physical_asap;
  const stage3 = doc.stage3_selection;
  if (stage1 !== undefined) errors.push(...validateCandidates(stage1 && stage1.candidates, 'stage1_logical_asap', false, null, queryCount));
  if (stage2 !== undefined) {
    if (stage1 === undefined) errors.push('stage2_physical_asap needs stage1_logical_asap');
    const logicalIds = new Set(((stage1 && stage1.candidates) || []).map((candidate) => candidate && candidate.id));
    errors.push(...validateCandidates(stage2 && stage2.candidates, 'stage2_physical_asap', true, logicalIds, queryCount));
  }
  if (stage3 !== undefined) {
    if (stage2 === undefined || !stage2 || !Array.isArray(stage2.candidates)) errors.push('stage3_selection needs stage2_physical_asap');
    else if (!stage3 || typeof stage3 !== 'object') errors.push('stage3_selection must be an object');
    else errors.push(...validateSelection(stage3, stage2.candidates));
  }
  return errors;
}

// Physical candidates with their Stage 3 outcome, cheapest first; a
// candidate Stage 3 did not cost sorts last. Costs come only from
// `stage3_selection.costs`, so without Stage 3 every row has no cost and
// status 'no_selection', in document order.
function rankPhysicalCandidates(doc) {
  const selection = doc.stage3_selection;
  const costs = (selection && selection.costs) || {};
  const rejected = new Map(((selection && selection.rejected) || []).map((entry) => [entry.id, entry]));
  const totalOf = (candidate) => (costs[candidate.id] ? costs[candidate.id].total : Infinity);
  let rank = 0;
  return ((doc.stage2_physical_asap && doc.stage2_physical_asap.candidates) || [])
    .map((candidate, order) => ({ candidate, order }))
    .sort((a, b) => (totalOf(a.candidate) - totalOf(b.candidate)) || a.order - b.order)
    .map(({ candidate }) => {
      const cost = costs[candidate.id];
      const entry = rejected.get(candidate.id);
      let status = 'no_selection';
      if (selection) status = candidate.id === selection.selected ? 'selected' : entry && entry.valid ? 'rejected_valid' : 'rejected_invalid';
      return {
        id: candidate.id,
        label: candidate.label || candidate.id,
        from_logical: candidate.from_logical,
        total: cost ? cost.total : null,
        unit: cost ? cost.unit : null,
        source: cost ? cost.source || null : null,
        rank: cost ? ++rank : null,
        status,
        reason: entry ? entry.reason || '' : null,
      };
    });
}

function snakeToPascal(text) {
  return String(text).split('_').map((part) => part.charAt(0).toUpperCase() + part.slice(1)).join('');
}

// The node-style.js kind name for a wire payload: relational operators use
// their inner `operator.kind`, ASAP operators their own `kind`.
function stagePayloadKind(payload) {
  const kind = payload && payload.kind === 'relational' ? payload.operator && payload.operator.kind : payload && payload.kind;
  if (kind === 'sql_window_func') return 'SQLWindowFunc';
  return snakeToPascal(kind || 'unknown');
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
  if (!keys || !Array.isArray(keys.keys) || keys.keys.length === 0) return null;
  return `${keys.without ? 'per group without' : 'per'} ${keys.keys.map((key) => wireColumn(key, schema)).join(', ')}`;
}

function formatTiming(timing) {
  const normalized = String(timing || '').replace(/_/g, '').toLowerCase();
  if (normalized === 'ingestiontime') return 'ingestion time';
  if (normalized === 'querytime') return 'query time';
  return String(timing || 'unknown');
}

// Box text from concrete payload fields; the sidebar shows the full payload.
function stageNodeLines(node, inputSchema) {
  const payload = node.payload || {};
  const op = payload.kind === 'relational' ? payload.operator || {} : payload;
  const lines = [stagePayloadKind(payload)];
  switch (op.kind) {
    case 'scan': lines.push(`source: ${wireSource(op.source)}`); break;
    case 'time_range': lines.push(`range: ${op.range ? op.range.secs : '?'}s`); break;
    case 'aggregate':
      (op.measures || []).forEach((measure) => {
        const args = [measure.col === null || measure.col === undefined ? '' : wireColumn(measure.col, inputSchema)]
          .concat(['q', 'k'].filter((key) => key in measure).map((key) => `${key}=${measure[key]}`))
          .filter(Boolean);
        lines.push(`measure: ${measure.kind}(${args.join(', ')})`);
      });
      if (wireGrouping(op.reduction, inputSchema)) lines.push(wireGrouping(op.reduction, inputSchema));
      break;
    case 'filter': lines.push(`where: ${compactWire(op.pred)}`); break;
    case 'sort':
      lines.push(`sort: ${(op.keys || []).map((key) => `${key.expr && typeof key.expr.Column === 'number' ? wireColumn(key.expr.Column, inputSchema) : compactWire(key.expr)} ${key.ascending ? 'asc' : 'desc'}`).join(', ')}`);
      if (wirePartition(op.partition_by, inputSchema)) lines.push(wirePartition(op.partition_by, inputSchema));
      break;
    case 'limit':
      lines.push(`rows: ${op.n === null || op.n === undefined ? 'all' : op.n}${op.offset ? `, offset ${op.offset}` : ''}`);
      if (wirePartition(op.partition_by, inputSchema)) lines.push(wirePartition(op.partition_by, inputSchema));
      break;
    case 'summary_agg': {
      const sketch = op.family && Array.isArray(op.family.Sketch) ? op.family.Sketch : null;
      lines.push(`family: ${sketch ? sketch[0].algorithm : compactWire(op.family)}`);
      if (wireGrouping(op.reduction, inputSchema)) lines.push(wireGrouping(op.reduction, inputSchema));
      const shared = op.grouping && op.grouping.SharedMultiSubpopulation;
      lines.push(`layout: ${shared ? shared.kind : compactWire(op.grouping)}`);
      break;
    }
    case 'summary_estimate': lines.push(`query: ${compactWire(op.query)}`); break;
    default: break;
  }
  return lines.slice(0, 4);
}

// One lane as cytoscape elements. `costPerNode` is the physical candidate's
// Stage 3 `per_node` map (absent for logical lanes and before Stage 3).
// `categoryFor` maps a kind name to a node-style.js category; `queryIds`
// names the workload queries, in root order.
function stageLaneElements(laneId, laneLabel, dag, options) {
  const { physical = false, costPerNode = null, categoryFor = () => 'unknown', queryIds = [] } = options || {};
  const byId = new Map(dag.nodes.map((node) => [node.id, node]));
  const inputOf = new Map();
  dag.edges.forEach((edge) => { if (!inputOf.has(edge.consumer)) inputOf.set(edge.consumer, byId.get(edge.producer)); });
  // node id -> ids of the workload queries it is the root of
  const rootFor = new Map();
  stageDagRoots(dag).forEach((root, index) => {
    const id = stageRootNodeId(root);
    if (id === null) return;
    if (!rootFor.has(id)) rootFor.set(id, []);
    rootFor.get(id).push(queryIds[index] || `query #${index + 1}`);
  });
  const elements = [
    { data: { id: laneId, label: laneLabel, isLane: true }, classes: 'laneParent', selectable: false, grabbable: false, pannable: true },
  ];
  dag.nodes.forEach((node) => {
    const kind = stagePayloadKind(node.payload);
    const input = inputOf.get(node.id);
    const lines = stageNodeLines(node, input && input.output_schema);
    const timing = physical && node.output_state ? node.output_state.timing : undefined;
    const cost = costPerNode ? costPerNode[String(node.id)] : undefined;
    if (timing !== undefined) lines.push(`⏱ ${formatTiming(timing)}`);
    if (cost && typeof cost.cost === 'number') lines.push(`cost ${Number(cost.cost.toFixed(3))}`);
    if (rootFor.has(node.id)) lines.push(`root of ${rootFor.get(node.id).join(', ')}`);
    elements.push({
      data: {
        id: `${laneId}-n${node.id}`,
        parent: laneId,
        label: lines.join('\n'),
        stageNode: node,
        kind,
        category: categoryFor(kind),
        root: rootFor.has(node.id),
        rootFor: rootFor.get(node.id) || [],
        laneId,
        physical,
        timing,
        nodeCost: cost,
      },
      classes: timing !== undefined && formatTiming(timing) === 'ingestion time' ? 'ingestionTime' : '',
    });
  });
  dag.edges.forEach((edge, index) => {
    elements.push({
      data: {
        id: `${laneId}-e${index}`,
        source: `${laneId}-n${edge.producer}`,
        target: `${laneId}-n${edge.consumer}`,
        stageEdge: edge,
        physical,
      },
    });
  });
  return elements;
}
