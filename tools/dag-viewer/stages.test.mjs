// Node tests of stages.js (no DOM): the document checks, the plan row and
// header, the queries that reach each node, the highlighted neighborhoods
// and the materialized views, over test-fixtures/p90-kll.json (the document
// asap_planner writes for test-fixtures/p90-workload.json; see
// memo.test.mjs) and, when asap_planner is built, over the plan it writes for
// #509 Example 3a (five p99 queries sharing one tumbling-window chain). The
// elements load into the vendored cytoscape (headless), which refuses
// duplicate ids and edges to missing nodes. Run from the repository root:
// `node --test tools/dag-viewer/stages.test.mjs`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { planned, planner } from './test-fixtures/planned.mjs';

const here = new URL('.', import.meta.url);
const context = vm.createContext({ console: { warn() {} } });
for (const file of ['node-style.js', 'stages.js']) {
  vm.runInContext(readFileSync(new URL(file, here), 'utf8'), context, { filename: file });
}
const call = (name, ...args) => JSON.parse(JSON.stringify(context[name](...args)));
// The vendored cytoscape, as the page loads it.
// Options are built inside its context: it checks for plain objects of its
// own realm.
const page = vm.createContext({ console, setTimeout, clearTimeout });
vm.runInContext(readFileSync(new URL('cytoscape.min.js', here), 'utf8'), page, { filename: 'cytoscape.min.js' });
const cytoscape = (elements) => {
  page.elementsJson = JSON.stringify(elements);
  return vm.runInContext('cytoscape({ headless: true, elements: JSON.parse(elementsJson) })', page);
};
const fixture = () => JSON.parse(readFileSync(new URL('test-fixtures/p90-kll.json', here), 'utf8'));
const nodesOf = (elements) => elements.filter((e) => !('source' in e.data));
const edgesOf = (elements) => elements.filter((e) => 'source' in e.data);
const example3a = planner ? planned('workloads/planner-layering/example3a.json') : null;

test('durations read largest unit first', () => {
  assert.equal(call('durationText', 580000), '9 min 40 s');
  assert.equal(call('durationText', 20000), '20 s');
  assert.equal(call('durationText', 3600000), '1 h');
});

test('the fixture is valid, with and without its memo section', () => {
  const doc = fixture();
  assert.deepEqual(call('validateStagePipeline', doc), []);
  delete doc.memo;
  assert.deepEqual(call('validateStagePipeline', doc), []);
});

test("the plan's lane draws each node with its timing and Stage 3 cost; cytoscape accepts it", () => {
  const doc = fixture();
  const { plan, cost } = doc.stage3_plan_selection;
  const elements = call('stageLaneElements', plan, { physical: true, costPerNode: cost.per_node, queryIds: ['q1'] });
  assert.equal(nodesOf(elements).length, plan.nodes.length);
  assert.equal(edgesOf(elements).length, plan.edges.length);
  for (const node of nodesOf(elements)) {
    assert.ok(node.data.label.includes('⏱ query time'), node.data.label);
    assert.equal(node.classes, 'queryTime');
    assert.deepEqual(node.data.cost, cost.per_node[String(node.data.nodeId)]);
    assert.deepEqual(node.data.reachedBy, ['q1']);
  }
  const kll = nodesOf(elements).find((n) => n.data.kind === 'SummaryAgg');
  assert.deepEqual(kll.data.label.split('\n').slice(0, 3), ['SummaryAgg', 'summary: Kll', 'per series']);
  assert.deepEqual(nodesOf(elements).filter((n) => n.data.rootFor.length).map((n) => n.data.nodeId), plan.roots);
  assert.equal(cytoscape(elements).nodes().length, plan.nodes.length);
});

test('Stage 0 nodes carry no timing; timing classes color the plan', () => {
  assert.equal(call('timingClass', 'ingestion_time'), 'ingestionTime');
  assert.equal(call('timingClass', 'QueryTime'), 'queryTime');
  assert.equal(call('timingClass', undefined), '');
  const elements = call('stageLaneElements', fixture().stage0_query_language_frontends.dag, { queryIds: ['q1'] });
  assert.ok(nodesOf(elements).every((n) => n.classes === '' && !n.data.label.includes('⏱')));
});

test('summary types read as their algorithm or exact kind', () => {
  assert.equal(call('summaryTypeText', { Sketch: 'Kll' }), 'Kll');
  assert.equal(call('summaryTypeText', { ExactAggregate: ['Sum', 'Sum'] }), 'exact Sum');
  const chain = { ASAP: { SummaryTumblingWindowChain: { child: 0, summary_type: { Sketch: 'Kll' }, reduction: 'PerEntity', tumbling_window_width_ms: 86400000 } } };
  assert.deepEqual(call('stageNodeLines', chain, null), ['SummaryTumblingWindowChain', 'summary: Kll', 'tumbling windows of 24 h', 'per series']);
});

test('the plan row shows the plan\'s cost and accepted accuracy, or selection\'s error', () => {
  const doc = fixture();
  assert.deepEqual(call('selectedPlanRow', doc), {
    status: 'selected', total: doc.stage3_plan_selection.cost.cost_per_second, accepted: 'q1: accepted rank (Kll)', reason: null,
  });
  doc.stage3_plan_selection = { plan: null, error: 'plan selection: no valid candidate', materialized_views: [] };
  assert.deepEqual(call('selectedPlanRow', doc), { status: 'no plan', total: null, accepted: null, reason: 'plan selection: no valid candidate' });
  assert.deepEqual(call('validateStagePipeline', doc), []);
  assert.equal(call('selectedPlanRow', call('fillStagesNotRun', { stage3_plan_selection: null })), null);
});

test('the header shows why a stage failed and the selection\'s warnings', () => {
  assert.equal(call('planHeader', fixture()), '');
  const failed = {
    stage1_logical_asap_optimization: { error: 'Pass 1: bad accuracy' },
    stage2_physical_asap_optimization: { error: 'no physical candidate' },
    stage3_plan_selection: { error: 'no plan', warnings: ['peak memory 1430000000000 bytes is over 1000000000000 bytes'] },
  };
  assert.equal(call('planHeader', failed), 'Stage 1 failed: Pass 1: bad accuracy · Stage 2 failed: no physical candidate · '
    + 'selection failed: no plan · warning: peak memory 1430000000000 bytes is over 1000000000000 bytes');
});

test('the counts line counts each stage\'s candidates and pruned candidates', () => {
  assert.equal(call('candidateCountsText', fixture()),
    'Candidates per stage: 17 logical (4 pruned by Pass 2) → 31 physical (16 pruned by Stage 2, 0 by Stage 3) → plan selected');
});

test('the search header names the search and the MILP\'s size and times', () => {
  const doc = fixture();
  const s3 = doc.stage3_plan_selection;
  assert.equal(call('searchText', doc), `candidate-connection search · MILP ${s3.milp_variables} variables, ${s3.milp_constraints} constraints`
    + ` · construction ${s3.milp_construction_ms} ms, solve ${s3.milp_solve_ms} ms`);
  assert.equal(call('searchText', { search: 'candidate-connections', stage3_plan_selection: { error: 'x' } }), 'candidate-connection search');
  assert.equal(call('searchText', {}), '');
});

test('queries show a fixed interval with or without a phase', () => {
  assert.equal(call('queryScheduleText', fixture().workload.queries[0]), 'every 1 min · predictable');
  assert.equal(call('queryScheduleText', { recurrence: { repeated: { fixed_interval_at: { interval: 60000, evaluation_phase: 15000 } } } }), 'every 1 min at phase 15 s');
  assert.equal(call('queryScheduleText', { recurrence: { repeated: { fixed_interval: 10000 } } }), 'every 10 s');
});

test('Stage 2 and Stage 3 pruning reasons read as words; dominated_by names its physical candidate', () => {
  assert.equal(call('prunedReasonText', 'query_time_results_not_retained'), 'query time results not retained');
  assert.equal(call('prunedReasonText', { dominated_by: 7 }), 'dominated by C7');
});

test('a materialized view row names its plan node, kind, refresh, windows, cost and the queries reading it', () => {
  const doc = fixture();
  const s3 = doc.stage3_plan_selection;
  const kll = s3.plan.nodes.find((n) => n.payload.ASAP && n.payload.ASAP.SummaryAgg);
  s3.materialized_views = [{ node_id: kll.id, refresh: 'at_ingestion', coverage: { definition_id: 'd1', selection: [] },
    inputs: { raw_data_windows: [{ end_offset_ms: 0, width_ms: 300000 }], materialized_views: [] }, retention_ms: 300000 }];
  assert.deepEqual(call('validateStagePipeline', doc), []);
  assert.deepEqual(call('materializedViewRows', doc), [{
    nodeId: kll.id, definition: 'd1', kind: 'SummaryAgg · Kll', refresh: 'at ingestion', inputs: '1 raw-data window(s)',
    retention: '5 min', window: 't−5 min … t', cost: s3.cost.per_node[String(kll.id)].detail, readBy: ['q1'],
  }]);
});

test('plan, view and per-node cost violations are reported', () => {
  const check = (edit) => { const doc = fixture(); edit(doc.stage3_plan_selection, doc); return call('validateStagePipeline', doc).join('\n'); };
  const view = { node_id: 0, refresh: 'at_ingestion', coverage: { definition_id: 'd' }, inputs: { raw_data_windows: [], materialized_views: [] }, retention_ms: 0 };
  assert.match(check((s3) => { s3.plan.roots.pop(); }), /plan: 0 roots and 0 scalar roots for 1 workload queries/);
  assert.match(check((s3) => { s3.plan.edges[0].producer = 99; }), /edge #0 names a missing producer or consumer/);
  assert.match(check((s3) => { s3.cost.per_node['99'] = {}; }), /cost.per_node names missing node 99/);
  assert.match(check((s3) => { s3.cost.cost_per_second = 'x'; }), /cost_per_second must be a number or null/);
  assert.equal(check((s3) => { s3.cost = null; }), '');
  assert.match(check((s3) => { s3.materialized_views = [{ ...view, node_id: 99 }]; }), /node_id must name a node of the plan/);
  assert.match(check((s3) => { s3.materialized_views = [{ ...view, refresh: 'later' }]; }), /refresh must be one of/);
  assert.match(check((s3) => { s3.materialized_views = [{ ...view, retention_ms: -1 }]; }), /retention_ms/);
  assert.match(check((s3) => { s3.plan = null; s3.materialized_views = [view]; }), /must be empty without a plan/);
  assert.match(check((s3, doc) => { doc.format = 'asap-stage-pipeline/v1'; }), /format must be/);
});

// Pass 2's, Stage 2's and Stage 3's candidate lists are checked: ids in
// range, the fields the memo view reads present.
test('pruned and physical candidate violations are reported', () => {
  const check = (edit) => { const doc = fixture(); edit(doc); return call('validateStagePipeline', doc).join('\n'); };
  const s1 = (doc) => doc.stage1_logical_asap_optimization;
  const s2 = (doc) => doc.stage2_physical_asap_optimization;
  const withInput = (doc) => s2(doc).physical_candidates.find((c) => c.inputs.length);
  assert.match(check((d) => { s1(d).pruned_candidates[0].causes = []; }), /stage1_logical_asap_optimization.pruned_candidates\[0\]/);
  assert.match(check((d) => { delete s1(d).pruned_candidates[0].causes[0].text; }), /causes, each with through_pruned_candidates, root and text/);
  assert.match(check((d) => { s1(d).pruned_candidates[0].causes[0].through_pruned_candidates = [{ group: 1 }]; }), /pruned_candidates\[0\]/);
  assert.match(check((d) => { s2(d).physical_candidates[1].id = 5; }), /physical_candidates\[1\]: needs id 1/);
  assert.match(check((d) => { withInput(d).inputs[0].edges[0].producer = 999; }), /inputs\[0\]: needs group and edges whose producer is a physical candidate/);
  assert.match(check((d) => { delete withInput(d).inputs[0].edges[0].satisfies_physical_requirement; }), /inputs\[0\]/);
  assert.match(check((d) => { delete s2(d).pruned_candidates[0].properties; }), /stage2_physical_asap_optimization.pruned_candidates\[0\]/);
  assert.match(check((d) => { d.stage3_plan_selection.pruned_candidates = [{ ...s2(d).pruned_candidates[0], reason: { dominated_by: 999 } }]; }),
    /stage3_plan_selection.pruned_candidates\[0\]/);
  assert.equal(check((d) => { d.stage3_plan_selection.pruned_candidates = [{ ...s2(d).pruned_candidates[0], reason: { dominated_by: 1 } }]; }), '');
  assert.match(check((d) => { delete s2(d).physical_candidates; }), /must be arrays/);
});

test('a node shared by several queries says so; a query keeps its own nodes and a node its neighborhood', { skip: !planner }, () => {
  const { plan } = example3a.stage3_plan_selection;
  const queryIds = example3a.workload.queries.map((q) => q.id);
  assert.deepEqual(call('validateStagePipeline', example3a), []);
  const elements = call('stageLaneElements', plan, { physical: true, queryIds });
  const chain = nodesOf(elements).find((e) => e.data.kind === 'SummaryTumblingWindowChain');
  assert.equal(chain.data.reachedBy.length, queryIds.length);
  assert.match(chain.data.label, new RegExp(`shared ×${queryIds.length}`));
  for (const [index, root] of plan.roots.entries()) {
    assert.ok(nodesOf(elements).find((e) => e.data.id === `n${root}`).data.reachedBy.includes(queryIds[index]));
  }
  const q1 = new Set(call('stageQueryElementIds', elements, queryIds[0]));
  assert.ok(q1.has(`n${plan.roots[0]}`) && q1.has(chain.data.id));
  assert.ok(!q1.has(`n${plan.roots[1]}`));
  const around = new Set(call('stageNeighborhoodIds', elements, chain.data.id));
  assert.ok(plan.roots.every((root) => around.has(`n${root}`)));
  const root = new Set(call('stageNeighborhoodIds', elements, `n${plan.roots[0]}`));
  assert.ok(root.has(chain.data.id) && !root.has(`n${plan.roots[1]}`));
  assert.equal(cytoscape(elements).nodes().length, nodesOf(elements).length);
});
