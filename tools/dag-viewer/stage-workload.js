// Reads an `asap-stage-pipeline/v1` document (the `stage_pipeline` bin, which
// server.py runs for the Query planner) as the viewer's workload: one entry
// per workload query, whose pre-ASAP DAG is its part of Stage 0 (the
// query-language frontends' DAG) and whose post-ASAP DAG is its part of the plan
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

// A standalone scalar query's root: not an operator, so a node of its own
// (`Scalar`, id `scalar:<query index>`) over the operator nodes it reads.
function stageScalarRoot(index, expr) {
  const id = `scalar:${index}`;
  return {
    id, kind: 'Scalar', workload_node_id: id, children: stageScalarRefs(expr),
    label: `Scalar\n${stageScalarText(expr)}`, schema: null, detail: { Scalar: expr },
  };
}

function stageLabel(kind, body, cost) {
  const lines = [kind];
  const source = body.source && Object.values(body.source)[0];
  if (kind === 'Scan' && source) lines.push(`source: ${source.metric || source.table_ref || JSON.stringify(source)}`);
  if (cost) lines.push(`cost ${Number(cost.cost.toPrecision(3))}${cost.multiplier === 1 ? '' : ` × ${cost.multiplier}`}`);
  return lines.join('\n');
}

// The nodes reachable from `root`, as viewer nodes, children first. A
// scalar root (`{ scalar: node }`) is its own node over what it reads.
function stageQueryDag(root, nodeOf) {
  const nodes = new Map();
  const visit = (id) => {
    if (nodes.has(id)) return;
    const node = nodeOf(id);
    nodes.set(id, null);
    node.children.forEach(visit);
    nodes.set(id, node);
  };
  if (root.scalar) {
    root.scalar.children.forEach(visit);
    nodes.set(root.scalar.id, root.scalar);
    return { nodes: Array.from(nodes.values()), root: root.scalar.id };
  }
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

  // The physical DAG keeps standalone scalar queries in `scalar_roots`,
  // keyed by query position; every other query is the next entry of `roots`.
  const physicalRootOf = new Map();
  if (selected) {
    const scalars = new Map((selected.dag.scalar_roots || []).map(([position, expr]) => [position, expr]));
    let next = 0;
    workloadQueries.forEach((_, index) => {
      physicalRootOf.set(index, scalars.has(index)
        ? { scalar: stageScalarRoot(index, scalars.get(index)) }
        : selected.dag.roots[next++]);
    });
  }

  return {
    queries: workloadQueries.map((query, index) => {
      const root = stage0.roots[index];
      let dag = { nodes: [], root: null };
      if (root && typeof root.Operator === 'number') dag = stageQueryDag(root.Operator, logicalNode);
      else if (root && 'Scalar' in root) dag = stageQueryDag({ scalar: stageScalarRoot(index, root.Scalar) }, logicalNode);
      const entry = { name: query.id, source: query.text, dag };
      if (physicalNode) entry.post_dag = stageQueryDag(physicalRootOf.get(index), physicalNode);
      return entry;
    }),
  };
}

// Stage 3's view of the candidates the document shows: the selected one,
// the other valid ones cheapest first ("costlier"), then the rejected and
// the Stage 2 pruned ones with their reasons, then the Stage 1 ones that
// did not compose ("composition error") with their errors.
function stageCandidateRows(doc) {
  const selection = doc.stage3_plan_selection || {};
  const costs = selection.costs || {};
  const rejected = new Map((selection.rejected || []).map((r) => [r.id, r]));
  const physical = doc.stage2_physical_asap_optimization || {};
  const rows = (physical.candidates || []).map((candidate) => {
    const cost = costs[candidate.id];
    const rejection = rejected.get(candidate.id);
    const status = candidate.id === selection.selected ? 'selected' : cost ? 'costlier' : 'rejected';
    return {
      id: candidate.id,
      display_name: candidate.display_name,
      total: cost ? cost.total : null,
      unit: cost ? cost.unit : null,
      status,
      reason: status === 'rejected' && rejection ? rejection.reason : null,
    };
  });
  (physical.pruned || []).forEach((p) => rows.push({
    id: p.from_logical, display_name: p.display_name, total: null, unit: null, status: 'pruned', reason: p.reason,
  }));
  ((doc.stage1_logical_asap_optimization || {}).candidates || []).filter((c) => 'error' in c).forEach((c) => rows.push({
    id: c.id, display_name: c.display_name, total: null, unit: null, status: 'composition error', reason: c.error,
  }));
  const order = { selected: 0, costlier: 1, rejected: 2, pruned: 3, 'composition error': 4 };
  return rows.sort((a, b) => order[a.status] - order[b.status] || (a.total ?? 0) - (b.total ?? 0));
}

// The candidate table's header: how many candidates Stage 1 enumerated,
// how many the document shows when it caps them, why Stage 1 failed (a
// Pass 1 error) and why plan selection failed when it failed for a reason
// other than rejecting every candidate.
function stageCandidateHeader(doc) {
  const logical = doc.stage1_logical_asap_optimization || {};
  const selection = doc.stage3_plan_selection || {};
  const shown = logical.capped ? ` (document shows ${(logical.candidates || []).length})` : '';
  const error = [
    logical.error ? ` · Stage 1 failed: ${logical.error}` : '',
    selection.error ? ` · selection failed: ${selection.error}` : '',
  ].join('');
  return `Stage 3 plan selection · ${logical.combinations ?? 0} candidates${shown}${error}`;
}
