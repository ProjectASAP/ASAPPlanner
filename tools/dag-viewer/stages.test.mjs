// Node tests of stages.js (no DOM): tumbling-window-chain expansion, the queries that
// reach each node, the highlighted neighborhoods and the grouped materialized
// outputs, over the plan asap_planner writes for test-fixtures/
// tumbling-windows-workload.json without raw data (q1 and q2 share a
// tumbling-window chain over m,
// q3 reads one over n; skipped when asap_planner is not built); the elements load
// into the vendored cytoscape (headless), which refuses duplicate ids and
// edges to missing nodes. Run from the repository root:
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
const call = (name, ...args) => context[name](...args);
// The vendored cytoscape, as the page loads it.
// Options are built inside its context: it checks for plain objects of its
// own realm.
const page = vm.createContext({ console, setTimeout, clearTimeout });
vm.runInContext(readFileSync(new URL('cytoscape.min.js', here), 'utf8'), page, { filename: 'cytoscape.min.js' });
const cytoscape = (elements) => {
  page.elementsJson = JSON.stringify(elements);
  return vm.runInContext('cytoscape({ headless: true, elements: JSON.parse(elementsJson) })', page);
};
const skip = !planner;
const doc = planner ? planned('test-fixtures/tumbling-windows-workload.json', { raw_data_retained: false, query_time_results_retained: false }) : null;
const dag = doc && doc.stage2_physical_asap_optimization.candidates[0].dag;
const queryIds = doc && doc.workload.queries.map((q) => q.id);
const chain = dag && dag.nodes.find((n) => n.payload.ASAP && n.payload.ASAP.SummaryTumblingWindowChain);
const body = chain && chain.payload.ASAP.SummaryTumblingWindowChain;
const nodesOf = (elements) => elements.filter((e) => !('source' in e.data));
const edgesOf = (elements) => elements.filter((e) => 'source' in e.data);

test('durations read largest unit first', () => {
  assert.equal(call('durationText', 580000), '9 min 40 s');
  assert.equal(call('durationText', 20000), '20 s');
  assert.equal(call('durationText', 3600000), '1 h');
  assert.equal(call('tumblingWindowTimeText', 0, 20000), 't−20 s … t');
  assert.equal(call('tumblingWindowTimeText', 2, 20000), 't−1 min … t−40 s');
});

test('a collapsed chain is one node; expanded, one build, range and shift per tumbling window', { skip }, () => {
  const collapsed = call('stageLaneElements', dag, { physical: true, queryIds });
  assert.ok(nodesOf(collapsed).some((e) => e.data.id === `n${chain.id}` && e.data.expandable));
  const expanded = call('stageLaneElements', dag, { physical: true, queryIds, expandedChains: [chain.id] });
  const tumblingWindows = nodesOf(expanded).filter((e) => e.data.tumblingWindowOf === chain.id);
  assert.equal(tumblingWindows.filter((e) => e.data.kind === 'tumblingWindow').length, body.tumbling_window_count);
  assert.equal(tumblingWindows.filter((e) => e.data.kind === 'TimeRange').length, body.tumbling_window_count);
  assert.equal(tumblingWindows.filter((e) => e.data.kind === 'TimeShift').length, body.tumbling_window_count - 1);
  // Each expanded node carries the one class app.js styles with a dashed border.
  assert.ok(tumblingWindows.every((e) => e.classes.split(' ').includes('tumblingWindow')));
  const ids = new Set(nodesOf(expanded).map((e) => e.data.id));
  for (const edge of edgesOf(expanded)) {
    assert.ok(ids.has(edge.data.source) && ids.has(edge.data.target), edge.data.id);
  }
  // Cytoscape accepts every chain expanded at once, and dims by element id.
  const every = dag.nodes.filter((n) => n.payload.ASAP && n.payload.ASAP.SummaryTumblingWindowChain).map((n) => n.id);
  const all = call('stageLaneElements', dag, { physical: true, queryIds, expandedChains: every });
  const cy = cytoscape(all);
  assert.equal(cy.nodes().length, nodesOf(all).length);
  const keep = new Set(call('stageQueryElementIds', all, 'q3'));
  cy.batch(() => cy.elements().forEach((el) => el.toggleClass('dimmed', !keep.has(el.id()))));
  assert.equal(cy.elements('.dimmed').length, all.length - keep.size);
});

test('a merge of the newest tumbling windows reads only those tumbling windows, and says so', { skip }, () => {
  const partial = JSON.parse(JSON.stringify(dag));
  const merge = partial.nodes.find((n) => n.payload.ASAP && n.payload.ASAP.SummaryMerge
    && partial.edges.some((e) => e.consumer === n.id && e.producer === chain.id));
  assert.ok(merge, 'a merge reads the chain');
  merge.payload.ASAP.SummaryMerge.newest_tumbling_windows = 2;
  const elements = call('stageLaneElements', partial, { physical: true, queryIds, expandedChains: [chain.id] });
  const into = edgesOf(elements).filter((e) => e.data.target === `n${merge.id}` && e.data.source.startsWith(`n${chain.id}p`));
  assert.equal(into.map((e) => e.data.source).sort().join(), `n${chain.id}p0,n${chain.id}p1`);
  const label = nodesOf(elements).find((e) => e.data.id === `n${merge.id}`).data.label;
  assert.match(label, /newest 2 tumbling windows/);
});

test('every node lists the queries reaching it; shared nodes say how many', { skip }, () => {
  const elements = call('stageLaneElements', dag, { physical: true, queryIds });
  const shared = nodesOf(elements).filter((e) => e.data.reachedBy.length > 1);
  assert.ok(shared.length > 0);
  for (const node of shared) assert.match(node.data.label, new RegExp(`shared ×${node.data.reachedBy.length}`));
  for (const [index, root] of dag.roots.entries()) {
    const node = nodesOf(elements).find((e) => e.data.id === `n${root}`);
    assert.ok(node.data.reachedBy.includes(queryIds[index]));
  }
});

test('a query keeps its own nodes and a node its neighborhood', { skip }, () => {
  const elements = call('stageLaneElements', dag, { physical: true, queryIds });
  const q3 = new Set(call('stageQueryElementIds', elements, 'q3'));
  assert.ok(q3.has(`n${dag.roots[2]}`));
  assert.ok(!q3.has(`n${chain.id}`));
  const around = new Set(call('stageNeighborhoodIds', elements, `n${chain.id}`));
  assert.ok(around.has(`n${body.child}`));
  assert.ok(around.has(`n${dag.roots[0]}`));
  assert.ok(!around.has(`n${dag.roots[2]}`));
});

test('materialized views group by tumbling-window chain', { skip }, () => {
  const groups = call('materializedViewGroups', doc);
  assert.equal(groups.length, 2);
  assert.equal(groups.reduce((n, g) => n + g.tumblingWindows.length, 0), doc.stage3_plan_selection.materialized_views.length);
  assert.equal(groups[0].tumblingWindowsText, `${body.tumbling_window_count} × 10 s`);
});

test('the candidate header shows the selection\'s warnings', () => {
  const warned = {
    stage1_logical_asap_optimization: { combinations: 2 },
    stage3_plan_selection: { warnings: ['peak memory 1430000000000 bytes is over 1000000000000 bytes'] },
  };
  assert.equal(
    call('stageCandidateHeader', warned),
    '2 candidates · warning: peak memory 1430000000000 bytes is over 1000000000000 bytes',
  );
  assert.equal(call('stageCandidateHeader', { stage1_logical_asap_optimization: { combinations: 2 } }), '2 candidates');
});

test('the physical candidate list is in plan-number order', () => {
  const ids = ['P10', 'P2-m1', 'P1', 'P3', 'P2'];
  assert.deepEqual(ids.sort((a, b) => call('byPlanNumber', a, b)), ['P1', 'P2', 'P2-m1', 'P3', 'P10']);
});

test('the search header names the search and its report', () => {
  const searched = {
    search: 'candidate-connections',
    stage3_plan_selection: { search_report: { status: 'time_limit', cost_lower_bound_per_second: null, milp_variables: 9, milp_constraints: 4,
      model_construction_ms: 1, solver_ms: 2, validation_ms: 3 } },
  };
  assert.equal(call('searchText', searched),
    'candidate-connection search · search time limit · MILP 9 variables, 4 constraints · model 1 ms, solve 2 ms, validation 3 ms');
  assert.equal(call('searchText', { search: 'candidate-connections', stage3_plan_selection: {} }), 'candidate-connection search');
  assert.equal(call('searchText', {}), '');
});

test('the validator accepts a document with and without its memo section', { skip }, () => {
  const memo = planned('memo-examples/promql/ratio-of-quantiles-5m.json');
  assert.deepEqual(memo.memo.steps.map((s) => s.step), ['stage0', 'pass1', 'pass2_local_window_composition', 'pass2']);
  assert.deepEqual([...call('validateStagePipeline', memo)], []);
  delete memo.memo;
  assert.deepEqual([...call('validateStagePipeline', memo)], []);
});

test('Stage 2 nodes carry their timing as a class the lane colors', { skip }, () => {
  assert.equal(call('timingClass', 'ingestion_time'), 'ingestionTime');
  assert.equal(call('timingClass', 'QueryTime'), 'queryTime');
  assert.equal(call('timingClass', undefined), '');
  const elements = call('stageLaneElements', dag, { physical: true, queryIds });
  const classes = new Set(nodesOf(elements).map((n) => n.classes));
  assert.ok(classes.has('ingestionTime') && classes.has('queryTime'));
  // The label keeps the timing in words.
  assert.ok(nodesOf(elements).filter((n) => n.classes === 'queryTime').every((n) => n.data.label.includes('⏱ query time')));
});

// Stage 2's physical and pruned candidates and Stage 3's pruned candidates
// are checked: ids in range, the fields the memo view reads present.
test('physical and pruned candidate violations are reported', () => {
  const edge = { producer: 0, satisfies_logical_requirement: true, satisfies_physical_requirement: true, rejection: null, selected: true };
  const stage2 = () => ({
    physical_candidates: [
      { id: 0, group: 0, index_in_group: 0, properties: { timing: 'ingestion_time', lifetime: 'None' }, inputs: [] },
      { id: 1, group: 1, index_in_group: 0, properties: { timing: 'query_time', lifetime: { OneEvaluation: 'Memory' } },
        inputs: [{ group: 0, requirement: { parameter_floor_for_required_accuracy: null }, edges: [edge] }] },
    ],
    pruned_candidates: [{ group: 1, index_in_group: 0, properties: { timing: 'ingestion_time', lifetime: 'None' }, reason: 'no_ingestion_pipeline' }],
  });
  const stage3 = () => ({ pruned_candidates: [{ group: 1, candidate: 1, reason: 'dominated_by', dominated_by: 0 }] });
  const check = (edit) => { const s2 = stage2(); const s3 = stage3(); edit(s2, s3); return [...call('validatePrunedAndPhysical', s2, s3)].join('\n'); };
  assert.equal(check(() => {}), '');
  assert.match(check((s2) => { s2.physical_candidates[1].id = 5; }), /physical_candidates\[1\]: needs id 1/);
  assert.match(check((s2) => { s2.physical_candidates[1].inputs[0].edges[0].producer = 7; }), /inputs\[0\]: needs group, requirement and edges/);
  assert.match(check((s2) => { delete s2.pruned_candidates[0].properties; }), /pruned_candidates\[0\]: needs group, index_in_group, properties and reason/);
  assert.match(check((s2, s3) => { s3.pruned_candidates[0].dominated_by = 9; }), /stage3_plan_selection.pruned_candidates\[0\]/);
  assert.match(check((s2) => { delete s2.physical_candidates; }), /must be arrays/);
});
