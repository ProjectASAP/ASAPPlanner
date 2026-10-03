// DOM-free logic for the Stages view: validating an `asap-stage-pipeline/v1`
// document, turning its LogicalASAPDAG / PhysicalASAPDAG lanes into
// cytoscape elements, and ranking physical candidates by cost. Kept apart
// from viewer.js so test_render.py can run it headless.
const STAGE_PIPELINE_FORMAT = 'asap-stage-pipeline/v1';

function isStagePipelineDocument(doc) {
  return !!doc && typeof doc === 'object' && doc.format === STAGE_PIPELINE_FORMAT;
}

// LogicalASAPDAG roots are `{"Operator": id}` or `{"Scalar": expr}`;
// PhysicalASAPDAG roots are a bare node id. A scalar root has no single
// operator node to mark, so it contributes none.
function stageRootIds(root) {
  if (typeof root === 'number') return [root];
  if (root && typeof root === 'object' && typeof root.Operator === 'number') return [root.Operator];
  return [];
}

function validateStageDag(dag, where, physical) {
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
  const isLogicalRoot = dag.root && typeof dag.root === 'object' && ('Operator' in dag.root || 'Scalar' in dag.root);
  if (physical ? typeof dag.root !== 'number' : !isLogicalRoot) errors.push(`${where}: malformed root`);
  stageRootIds(dag.root).forEach((id) => { if (!ids.has(id)) errors.push(`${where}: root ${id} does not name a node`); });
  return errors;
}

function validateCandidates(list, where, physical, logicalIds) {
  const errors = [];
  if (!Array.isArray(list) || list.length === 0) return [`${where}.candidates must be a non-empty array`];
  const ids = new Set();
  list.forEach((candidate, index) => {
    const at = `${where}.candidates[${index}]`;
    if (!candidate || typeof candidate.id !== 'string' || !candidate.id) { errors.push(`${at}: id must be a non-empty string`); return; }
    if (ids.has(candidate.id)) errors.push(`${at}: duplicate candidate id ${candidate.id}`);
    ids.add(candidate.id);
    errors.push(...validateStageDag(candidate.dag, `${where} ${candidate.id}`, physical));
    if (!physical) return;
    if (!logicalIds.has(candidate.from_logical)) errors.push(`${at}: from_logical ${JSON.stringify(candidate.from_logical)} is not a stage-1 candidate`);
    const cost = candidate.cost;
    if (!cost || typeof cost.total !== 'number' || !Number.isFinite(cost.total)) errors.push(`${at}: cost.total must be a number`);
    if (!cost || typeof cost.unit !== 'string') errors.push(`${at}: cost.unit must be a string`);
    const nodeIds = new Set(((candidate.dag && candidate.dag.nodes) || []).map((node) => String(node.id)));
    Object.keys((cost && cost.per_node) || {}).forEach((key) => {
      if (!nodeIds.has(key)) errors.push(`${at}: cost.per_node names missing node ${key}`);
    });
  });
  return errors;
}

// Every shape problem, as readable strings; empty means the document can be
// rendered. The viewer refuses a document with errors rather than guessing.
function validateStagePipeline(doc) {
  if (!isStagePipelineDocument(doc)) return [`format must be ${JSON.stringify(STAGE_PIPELINE_FORMAT)}`];
  const errors = [];
  errors.push(...validateStageDag(doc.stage0_logical && doc.stage0_logical.dag, 'stage0_logical', false));
  const logical = (doc.stage1_logical_asap && doc.stage1_logical_asap.candidates) || [];
  errors.push(...validateCandidates(doc.stage1_logical_asap && doc.stage1_logical_asap.candidates, 'stage1_logical_asap', false));
  const logicalIds = new Set(logical.map((candidate) => candidate && candidate.id));
  const physical = (doc.stage2_physical_asap && doc.stage2_physical_asap.candidates) || [];
  errors.push(...validateCandidates(doc.stage2_physical_asap && doc.stage2_physical_asap.candidates, 'stage2_physical_asap', true, logicalIds));
  const physicalIds = new Set(physical.map((candidate) => candidate && candidate.id));
  const selection = doc.stage3_selection;
  if (!selection || !physicalIds.has(selection.selected)) {
    errors.push('stage3_selection.selected must name a stage-2 candidate');
  } else {
    const rejected = new Set();
    (Array.isArray(selection.rejected) ? selection.rejected : []).forEach((entry, index) => {
      const id = entry && entry.id;
      if (!physicalIds.has(id)) errors.push(`stage3_selection.rejected[${index}]: ${JSON.stringify(id)} is not a stage-2 candidate`);
      if (id === selection.selected) errors.push(`stage3_selection.rejected[${index}]: ${id} is also selected`);
      if (rejected.has(id)) errors.push(`stage3_selection.rejected[${index}]: ${id} is rejected twice`);
      rejected.add(id);
    });
    if (selection.rejected !== undefined && !Array.isArray(selection.rejected)) errors.push('stage3_selection.rejected must be an array');
  }
  return errors;
}

// Physical candidates cheapest first. Status comes only from
// stage3_selection; a candidate it neither selects nor rejects is
// reported as such, not assumed rejected.
function rankPhysicalCandidates(doc) {
  const selection = doc.stage3_selection || {};
  const reasons = new Map((selection.rejected || []).map((entry) => [entry.id, entry.reason || '']));
  return doc.stage2_physical_asap.candidates
    .map((candidate, order) => ({ candidate, order }))
    .sort((a, b) => a.candidate.cost.total - b.candidate.cost.total || a.order - b.order)
    .map(({ candidate }, index) => ({
      id: candidate.id,
      label: candidate.label || candidate.id,
      from_logical: candidate.from_logical,
      total: candidate.cost.total,
      unit: candidate.cost.unit,
      rank: index + 1,
      status: candidate.id === selection.selected ? 'selected' : reasons.has(candidate.id) ? 'rejected' : 'not_selected',
      reason: reasons.get(candidate.id),
    }));
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
    case 'limit': lines.push(`rows: ${op.n}`); break;
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
// `cost.per_node` map (absent for logical lanes). `categoryFor` maps a kind
// name to a node-style.js category.
function stageLaneElements(laneId, laneLabel, dag, options) {
  const { physical = false, costPerNode = null, categoryFor = () => 'unknown' } = options || {};
  const byId = new Map(dag.nodes.map((node) => [node.id, node]));
  const inputOf = new Map();
  dag.edges.forEach((edge) => { if (!inputOf.has(edge.consumer)) inputOf.set(edge.consumer, byId.get(edge.producer)); });
  const roots = new Set(stageRootIds(dag.root));
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
    elements.push({
      data: {
        id: `${laneId}-n${node.id}`,
        parent: laneId,
        label: lines.join('\n'),
        stageNode: node,
        kind,
        category: categoryFor(kind),
        root: roots.has(node.id),
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
