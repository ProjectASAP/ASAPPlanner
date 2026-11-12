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

test('queries show a fixed interval and its phase', () => {
  assert.equal(call('queryScheduleText', { recurrence: { repeated: { fixed_interval_at: { interval: 60000, evaluation_phase: 0 } } } }), 'every 1 min');
  assert.equal(call('queryScheduleText', { recurrence: { repeated: { fixed_interval_at: { interval: 60000, evaluation_phase: 15000 } } } }), 'every 1 min at phase 15 s');
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

test('the search header says why the search ran as one and which queries were interchangeable', () => {
  const searched = {
    search: 'default',
    workload: { queries: [{ id: 'p50' }, { id: 'p90' }, { id: 'p99' }, { id: 'total' }] },
    stage3_plan_selection: {
      search: {
        candidates_checked: 12, queries: [], components: [], raw_scan_candidates_skipped: 0, raw_scan_assignments_skipped: 0,
        searched_as_one: 'not_in_joint_space',
        interchangeable_queries: [[0, 1, 2]],
      },
    },
  };
  assert.equal(
    call('searchText', searched),
    'default search · 12 physical candidates checked · searched as one: not in joint space · interchangeable queries: (p50, p90, p99)',
  );
  // Components searched separately carry no reason; no classes, no text.
  searched.stage3_plan_selection.search.searched_as_one = null;
  searched.stage3_plan_selection.search.interchangeable_queries = undefined;
  assert.equal(call('searchText', searched), 'default search · 12 physical candidates checked');
});

test('the validator accepts a document with and without its memo section', { skip }, () => {
  const memo = planned('memo-examples/promql/ratio-of-quantiles-5m.json');
  assert.deepEqual(memo.memo.steps.map((s) => s.step), ['stage0', 'pass1', 'pass2_local_window_composition', 'pass2']);
  assert.deepEqual([...call('validateStagePipeline', memo)], []);
  delete memo.memo;
  assert.deepEqual([...call('validateStagePipeline', memo)], []);
});

test('a document of another search has no memo-MILP view', { skip }, () => {
  assert.equal(call('memoSearchView', doc), null);
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

// Current reports retain nullable bounds and cannot certify a timeout incumbent.
test('candidate search report exposes objective and partial status', () => {
  const document = { search: 'candidate-connections', workload: { queries: [] },
    stage3_plan_selection: { search: { candidates_checked: 3, incumbent_cost: 2,
      candidate_connections: { status: 'time_limit', objective: 'cluster_capacity',
        calibration_scope: 'frozen', lower_bound: null, relative_gap: null,
        refinements: 3, solver_ms: 4 } } } };
  const view = call('memoSearchView', document);
  assert.equal(view.summary.certified, false);
  assert.equal(view.summary.lowerBound, null);
  assert.equal(view.summary.objective, 'cluster_capacity');
  assert.match(call('searchText', document), /candidate-connection search/);
});

test('a document written before the tumbling-window rename (#1163) upgrades to today\'s names', { skip }, () => {
  // Today's document, written back with the old names.
  const old = JSON.parse(JSON.stringify(doc)
    .replaceAll('"SummaryTumblingWindowChain"', '"PaneChain"')
    .replaceAll('"tumbling_window_width_ms"', '"pane_width_ms"')
    .replaceAll('"tumbling_window_count"', '"pane_count"')
    .replaceAll('"newest_tumbling_windows"', '"newest_panes"')
    .replaceAll('"tumbling_window_retention_ms"', '"pane_retention_ms"')
    .replaceAll('"tumbling_window_width_domain"', '"pane_width_domain"')
    .replaceAll('"tumbling_window_widths_pending"', '"pane_widths_pending"'));
  assert.ok(JSON.stringify(old).includes('"PaneChain"'));
  assert.deepEqual(call('upgradeTumblingWindowNames', old), doc);
});

// A document written before `MemoSteps` was dropped opens with today's
// steps: the window-composition step is gone and Pass 2's output is `pass2`.
test('a memo written with the window-composition step upgrades to stage0, pass1, pass2', () => {
  const old = {
    stage1_logical_asap_optimization: { pass1_pruned: [{ group: 1 }] },
    memo: {
      steps: [{ step: 'stage0', title: 'S0' }, { step: 'pass1', title: 'P1' }, { step: 'pass2', title: 'windows' }, { step: 'concrete_states', title: 'rules' }],
      enumeration: { step: 'concrete_states', per_step: { stage0: 1, pass1: 2, pass2: 3, concrete_states: 4 }, choices: [] },
    },
  };
  const doc = JSON.parse(JSON.stringify(call('upgradeMemoSteps', old)));
  assert.deepEqual(doc.memo.steps.map((s) => [s.step, s.title]), [['stage0', 'S0'], ['pass1', 'P1'], ['pass2', 'Stage 1, Pass 2']]);
  assert.deepEqual(doc.memo.enumeration.per_step, { stage0: 1, pass1: 2, pass2: 4 });
  assert.equal(doc.memo.enumeration.step, 'pass2');
  assert.deepEqual(doc.stage1_logical_asap_optimization, { pruned_candidates: [{ group: 1 }] });
  // A candidate the dropped step added stays marked new in `pass2`.
  const marked = JSON.parse(JSON.stringify(call('upgradeMemoSteps', {
    memo: {
      steps: [
        { step: 'stage0', groups: [] },
        { step: 'pass2', groups: [{ id: 2, candidates: [{ node: 5, children: [1], added: true }] }] },
        { step: 'concrete_states', groups: [{ id: 2, candidates: [{ node: 5, children: [1], added: false }, { node: 6, children: [], added: false }] }] },
      ],
    },
  })));
  assert.deepEqual(marked.memo.steps[1].groups[0].candidates.map((c) => c.added), [true, false]);
  // An old listing of the dropped step lists nothing; the document stays valid.
  const listedDropped = {
    memo: {
      steps: [{ step: 'stage0' }, { step: 'pass1' }, { step: 'pass2' }, { step: 'concrete_states' }],
      enumeration: { step: 'pass2', limit: 10, per_step: { stage0: { count: 1, complete: true }, pass2: { count: 2, complete: true }, concrete_states: { count: 9, complete: false } }, choices: [{ id: 'E1' }], priced: { count: 1 } },
    },
  };
  const enumeration = JSON.parse(JSON.stringify(call('upgradeMemoSteps', listedDropped))).memo.enumeration;
  assert.equal(enumeration.step, 'pass2');
  assert.deepEqual(enumeration.choices, []);
  assert.equal(enumeration.priced, null);
  assert.deepEqual(enumeration.per_step, { stage0: { count: 1, complete: true }, pass2: { count: 9, complete: false } });
  // A current document is left as it is.
  const current = { memo: { steps: [{ step: 'stage0' }, { step: 'pass1' }, { step: 'pass2' }] } };
  assert.deepEqual(JSON.parse(JSON.stringify(call('upgradeMemoSteps', JSON.parse(JSON.stringify(current))))), current);
});

test('a memo written before the rename upgrades its pane origin', () => {
  const group = { candidate_count_by_origin: { pass1: 1, panes: 2 }, candidates: [{ origin: 'pass1' }, { origin: 'panes' }] };
  assert.deepEqual(call('upgradeTumblingWindowNames', group),
    { candidate_count_by_origin: { pass1: 1, tumbling_windows: 2 }, candidates: [{ origin: 'pass1' }, { origin: 'tumbling_windows' }] });
});

test('the upgrade keeps key order, never lets an old key overwrite a new one, and renames old MILP report origins', () => {
  const body = { family: 'f', pane_width_ms: 1, pane_count: 2, child: 0 };
  call('upgradeTumblingWindowNames', body);
  assert.deepEqual(Object.keys(body), ['family', 'tumbling_window_width_ms', 'tumbling_window_count', 'child']);
  const both = { pane_count: 1, tumbling_window_count: 2 };
  call('upgradeTumblingWindowNames', both);
  assert.equal(both.tumbling_window_count, 2);
  const report = { candidates_by_origin: { Pass1: 1, Panes: 3 }, chosen: { origin: 'Panes' } };
  assert.deepEqual(call('upgradeTumblingWindowNames', report),
    { candidates_by_origin: { Pass1: 1, TumblingWindows: 3 }, chosen: { origin: 'TumblingWindows' } });
});
