// Node tests of stages.js (no DOM): pane-chain expansion, the queries that
// reach each node, the highlighted neighborhoods and the grouped materialized
// outputs, over the committed seed-862 workload example; the elements load
// into the vendored cytoscape (headless), which refuses duplicate ids and
// edges to missing nodes. Run from the repository root:
// `node --test tools/dag-viewer/stages.test.mjs`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

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
const doc = JSON.parse(readFileSync(new URL('examples/random-mixed-seed-862-workload-01.json', here), 'utf8'));
const dag = doc.stage2_physical_asap_optimization.candidates[0].dag;
const queryIds = doc.workload.queries.map((q) => q.id);
const chain = dag.nodes.find((n) => n.payload.ASAP && n.payload.ASAP.PaneChain);
const body = chain.payload.ASAP.PaneChain;
const nodesOf = (elements) => elements.filter((e) => !('source' in e.data));
const edgesOf = (elements) => elements.filter((e) => 'source' in e.data);

test('durations read largest unit first', () => {
  assert.equal(call('durationText', 580000), '9 min 40 s');
  assert.equal(call('durationText', 20000), '20 s');
  assert.equal(call('durationText', 3600000), '1 h');
  assert.equal(call('paneWindowText', 0, 20000), 't−20 s … t');
  assert.equal(call('paneWindowText', 2, 20000), 't−1 min … t−40 s');
});

test('a collapsed chain is one node; expanded, one build, range and shift per pane', () => {
  const collapsed = call('stageLaneElements', dag, { physical: true, queryIds });
  assert.ok(nodesOf(collapsed).some((e) => e.data.id === `n${chain.id}` && e.data.expandable));
  const expanded = call('stageLaneElements', dag, { physical: true, queryIds, expandedChains: [chain.id] });
  const panes = nodesOf(expanded).filter((e) => e.data.paneOf === chain.id);
  assert.equal(panes.filter((e) => e.data.kind === 'pane').length, body.pane_count);
  assert.equal(panes.filter((e) => e.data.kind === 'TimeRange').length, body.pane_count);
  assert.equal(panes.filter((e) => e.data.kind === 'TimeShift').length, body.pane_count - 1);
  const ids = new Set(nodesOf(expanded).map((e) => e.data.id));
  for (const edge of edgesOf(expanded)) {
    assert.ok(ids.has(edge.data.source) && ids.has(edge.data.target), edge.data.id);
  }
  // Cytoscape accepts every chain expanded at once, and dims by element id.
  const every = dag.nodes.filter((n) => n.payload.ASAP && n.payload.ASAP.PaneChain).map((n) => n.id);
  const all = call('stageLaneElements', dag, { physical: true, queryIds, expandedChains: every });
  const cy = cytoscape(all);
  assert.equal(cy.nodes().length, nodesOf(all).length);
  const keep = new Set(call('stageQueryElementIds', all, 'q9'));
  cy.batch(() => cy.elements().forEach((el) => el.toggleClass('dimmed', !keep.has(el.id()))));
  assert.equal(cy.elements('.dimmed').length, all.length - keep.size);
});

test('a merge of the newest panes reads only those panes, and says so', () => {
  const partial = JSON.parse(JSON.stringify(dag));
  const merge = partial.nodes.find((n) => n.payload.ASAP && n.payload.ASAP.SummaryMerge
    && partial.edges.some((e) => e.consumer === n.id && e.producer === chain.id));
  assert.ok(merge, 'a merge reads the chain');
  merge.payload.ASAP.SummaryMerge.newest_panes = 2;
  const elements = call('stageLaneElements', partial, { physical: true, queryIds, expandedChains: [chain.id] });
  const into = edgesOf(elements).filter((e) => e.data.target === `n${merge.id}` && e.data.source.startsWith(`n${chain.id}p`));
  assert.equal(into.map((e) => e.data.source).sort().join(), `n${chain.id}p0,n${chain.id}p1`);
  const label = nodesOf(elements).find((e) => e.data.id === `n${merge.id}`).data.label;
  assert.match(label, /newest 2 panes/);
});

test('every node lists the queries reaching it; shared nodes say how many', () => {
  const elements = call('stageLaneElements', dag, { physical: true, queryIds });
  const shared = nodesOf(elements).filter((e) => e.data.reachedBy.length > 1);
  assert.ok(shared.length > 0);
  for (const node of shared) assert.match(node.data.label, new RegExp(`shared ×${node.data.reachedBy.length}`));
  for (const [index, root] of dag.roots.entries()) {
    const node = nodesOf(elements).find((e) => e.data.id === `n${root}`);
    assert.ok(node.data.reachedBy.includes(queryIds[index]));
  }
});

test('a query keeps its own nodes and a node its neighborhood', () => {
  const elements = call('stageLaneElements', dag, { physical: true, queryIds });
  const q9 = new Set(call('stageQueryElementIds', elements, 'q9'));
  assert.ok(q9.has(`n${dag.roots[8]}`));
  assert.ok(!q9.has(`n${chain.id}`));
  const around = new Set(call('stageNeighborhoodIds', elements, `n${chain.id}`));
  assert.ok(around.has(`n${body.child}`));
  assert.ok(around.has(`n${dag.roots[0]}`));
  assert.ok(!around.has(`n${dag.roots[8]}`));
});

test('materialized views group by pane chain', () => {
  const groups = call('materializedViewGroups', doc);
  assert.equal(groups.length, 6);
  assert.equal(groups.reduce((n, g) => n + g.panes.length, 0), doc.stage3_plan_selection.materialized_views.length);
  assert.equal(groups[0].panesText, `${body.pane_count} × 20 s`);
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

// The memo-MILP examples: the MILP summary, the memo groups with their
// chosen candidates and the shared units with readers and threshold.
const memoDoc = (id) => JSON.parse(readFileSync(new URL(`examples/memo-${id}.json`, here), 'utf8'));

test('a memo-MILP document shows its shared sketch, readers and certified summary', () => {
  const doc = memoDoc('rangeheavy-t5-q05-q09-1h');
  assert.deepEqual([...call('validateStagePipeline', doc)], []);
  assert.match(call('searchText', doc), /^memo-MILP search · /);
  const view = call('memoSearchView', doc);
  assert.equal(view.summary.certified, true);
  assert.ok(view.summary.saving > 0.4);
  assert.match(call('memoSummaryText', view.summary), /certified \(gap ≤ 1e-4\).*queries planned alone .* \(sharing saves 50%\)/);
  const chain = view.sharedUnits.find((u) => u.kind === 'coupling site · PaneChain');
  assert.deepEqual([...chain.readers], ['G3', 'G4']);
  assert.deepEqual([...chain.queries], ['q1', 'q2']);
  assert.match(chain.threshold, /^1 min panes over 1 h · DDSketch · alpha 0.05/);
  assert.equal(view.groups.find((g) => g.id === 'G3').chosen, 'Panes · Project · at S1');
});

test('the validator accepts a document with and without its memo section', () => {
  const doc = memoDoc('ratio-of-quantiles');
  assert.deepEqual(doc.memo.steps.map((s) => s.step), ['stage0', 'pass1', 'pass2', 'concrete_states']);
  assert.deepEqual([...call('validateStagePipeline', doc)], []);
  delete doc.memo;
  assert.deepEqual([...call('validateStagePipeline', doc)], []);
});

test('a memo-MILP document shows a sketch rollup, a union schedule and a widened scan', () => {
  const rollup = call('memoSearchView', memoDoc('rollupheavy-t11-service-endpoint-5m'));
  assert.match(rollup.groups.find((g) => g.id === 'G3').chosen, /^Rollup · Project · reads G2, G6 · at S1$/);
  assert.match(rollup.sharedUnits[0].threshold, /^finer state Kmv · k 3001/);
  const union = call('memoSearchView', memoDoc('rangeheavy-t5-q05-1h-60s-300s'));
  assert.equal(union.sharedUnits[0].schedule, 'every 1 min ∪ every 5 min → on a 1 min grid');
  const widened = call('memoSearchView', memoDoc('rangeheavy-t7-t4-1h'));
  assert.equal(widened.scansWidened, true);
  assert.equal(widened.sharedUnits[0].kind, 'widened scan');
});

test('a memo plan naming a missing group is refused', () => {
  const broken = memoDoc('rangeheavy-t5-q05-q09-1h');
  broken.stage3_plan_selection.search.memo_plan.coupling_sites[0].readers.push(99);
  assert.match(call('validateStagePipeline', broken)[0], /coupling_sites\[0\]: readers must name groups/);
  assert.equal(call('memoSearchView', memoDoc('rangeheavy-t5-q05-q09-1h')).groups.length, 4);
  // Other searches have no memo view.
  assert.equal(call('memoSearchView', doc), null);
});

test('Stage 2 nodes carry their timing as a class the lane colors', () => {
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
