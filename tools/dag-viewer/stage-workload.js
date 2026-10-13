// Reads an `asap-stage-pipeline/v1` document (the `stage_pipeline` bin, which
// server.py runs for the Query planner) as the viewer's workload: one entry
// per workload query, whose pre-ASAP DAG is its part of Stage 0 (the query
// language frontends' DAG) and whose post-ASAP DAG is its part of the plan
// Stage 3 selected (a Stage 2 physical DAG), each node labeled with its
// Stage 3 cost. A node shared by several queries keeps its stage DAG id as
// `workload_node_id`, so a multi-query selection shows it once. DOM-free, so
// test_render.py runs it headless.
const STAGE_PIPELINE_FORMAT = 'asap-stage-pipeline/v1';

function isStagePipelineDocument(doc) {
  return !!doc && typeof doc === 'object' && doc.format === STAGE_PIPELINE_FORMAT;
}

// `{"NonASAP": {"Scan": {...}}}` → ["Scan", {...}]; a unit variant has no body.
function stageOperatorParts(operator) {
  const inner = Object.values(operator)[0];
  if (typeof inner === 'string') return [inner, {}];
  const [kind, body] = Object.entries(inner)[0];
  return [kind, body && typeof body === 'object' && !Array.isArray(body) ? body : { value: body }];
}

// Node ids a logical operator reads: its inputs, then scalar subqueries.
const STAGE_CHILD_FIELDS = ['child', 'children', 'left', 'right', 'summary_input', 'outer', 'inner'];

function stageLogicalChildren(body) {
  const ids = [];
  STAGE_CHILD_FIELDS.forEach((field) => {
    [].concat(body[field] ?? []).forEach((id) => { if (typeof id === 'number') ids.push(id); });
  });
  const walk = (value) => {
    if (!value || typeof value !== 'object') return;
    if (typeof value.ScalarSubquery === 'number') ids.push(value.ScalarSubquery);
    Object.values(value).forEach(walk);
  };
  Object.entries(body).forEach(([field, value]) => { if (!STAGE_CHILD_FIELDS.includes(field)) walk(value); });
  return ids;
}

function stageLabel(kind, body, cost) {
  const lines = [kind];
  const source = body.source && Object.values(body.source)[0];
  if (kind === 'Scan' && source) lines.push(`source: ${source.metric || source.table_ref || JSON.stringify(source)}`);
  if (cost) lines.push(`cost ${Number(cost.cost.toPrecision(3))}${cost.multiplier === 1 ? '' : ` × ${cost.multiplier}`}`);
  return lines.join('\n');
}

// The nodes reachable from `root`, as viewer nodes, children first.
function stageQueryDag(root, nodeOf) {
  const nodes = new Map();
  const visit = (id) => {
    if (nodes.has(id)) return;
    const node = nodeOf(id);
    nodes.set(id, null);
    node.children.forEach(visit);
    nodes.set(id, node);
  };
  visit(root);
  return { nodes: Array.from(nodes.values()), root };
}

function stageWorkload(doc) {
  const workloadQueries = (doc.workload && doc.workload.queries) || [];
  const stage0 = doc.stage0_query_language_frontends.dag;
  const logicalNode = (id) => {
    const node = stage0.nodes[id];
    const [kind, body] = stageOperatorParts(node.operator);
    return {
      id, kind, workload_node_id: id, children: stageLogicalChildren(body),
      label: stageLabel(kind, body), schema: node.schema, detail: node,
    };
  };

  const selection = doc.stage3_plan_selection || {};
  const selected = (doc.stage2_physical_asap_optimization.candidates || []).find((c) => c.id === selection.selected);
  let physicalNode = null;
  if (selected) {
    const perNode = (selection.costs[selected.id] || {}).per_node || {};
    const byId = new Map(selected.dag.nodes.map((node) => [node.id, node]));
    const inputs = new Map();
    selected.dag.edges.forEach((edge) => {
      if (!inputs.has(edge.consumer)) inputs.set(edge.consumer, []);
      inputs.get(edge.consumer).push(edge.producer);
    });
    physicalNode = (id) => {
      const node = byId.get(id);
      const [kind, body] = stageOperatorParts(node.payload);
      const cost = perNode[String(id)];
      return {
        id, kind, workload_node_id: id, children: inputs.get(id) || [],
        label: stageLabel(kind, body, cost), schema: node.output_schema,
        detail: cost ? { ...node, cost } : node,
      };
    };
  }

  return {
    queries: workloadQueries.map((query, index) => {
      const root = stage0.roots[index];
      const entry = {
        name: query.id,
        source: query.text,
        dag: root && typeof root.Operator === 'number' ? stageQueryDag(root.Operator, logicalNode) : { nodes: [], root: null },
      };
      if (physicalNode) entry.post_dag = stageQueryDag(selected.dag.roots[index], physicalNode);
      return entry;
    }),
  };
}
