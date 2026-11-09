// Node tests of memo.js (no DOM): the `memo` section checks, the per-step
// diff, a step's elements (groups holding their candidates, collapsed large
// groups, the selected and pruned candidates, query dimming), a candidate's
// θ and required accuracy, its logical edges, its physical candidates'
// edges and the pruned candidates of Pass 2, Stage 2 and Stage 3, over
// test-fixtures/p90-kll.json: the document asap_planner writes for
// test-fixtures/p90-workload.json under test-fixtures/kll-only-capabilities.json
// (one p90 query with accuracy enforced; Pass 2 prunes G3's four smallest
// KLL sketches). The elements load into the vendored cytoscape (headless).
// Run from the repository root: `node --test tools/dag-viewer/memo.test.mjs`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const here = new URL('.', import.meta.url);
const context = vm.createContext({ console: { warn() {} } });
for (const file of ['node-style.js', 'stages.js', 'memo.js']) {
  vm.runInContext(readFileSync(new URL(file, here), 'utf8'), context, { filename: file });
}
// Results cross back from the context as plain JSON (arrays of another realm
// are not deep-equal to this one's).
const call = (name, ...args) => JSON.parse(JSON.stringify(context[name](...args)));
const page = vm.createContext({ console, setTimeout, clearTimeout });
vm.runInContext(readFileSync(new URL('cytoscape.min.js', here), 'utf8'), page, { filename: 'cytoscape.min.js' });
const cytoscape = (elements) => {
  page.elementsJson = JSON.stringify(elements);
  return vm.runInContext('cytoscape({ headless: true, elements: JSON.parse(elementsJson), layout: { name: "preset" } })', page);
};
const fixtureDoc = () => JSON.parse(readFileSync(new URL('test-fixtures/p90-kll.json', here), 'utf8'));
const fixture = () => fixtureDoc().memo;
const nodesOf = (elements) => elements.filter((e) => !('source' in e.data));
const edgesOf = (elements) => elements.filter((e) => 'source' in e.data);
const byId = (elements, id) => elements.find((e) => e.data.id === id);
const PASS2 = 2;

test('the fixture is a valid memo; a document without one has nothing to check', () => {
  assert.deepEqual(call('validateMemo', fixture(), 1), []);
  assert.deepEqual(call('validateMemo', undefined, 1), []);
});

test('memo violations are reported', () => {
  const broken = (edit) => { const memo = fixture(); edit(memo); return call('validateMemo', memo, 1).join('\n'); };
  assert.match(broken((m) => { m.steps = []; }), /memo.steps must be a non-empty array/);
  assert.match(broken((m) => { m.steps[1].step = 'stage0'; }), /out of pipeline order or repeated/);
  assert.match(broken((m) => { m.steps[0].step = 'pass3'; }), /step must be one of/);
  assert.match(broken((m) => { m.steps[0].groups[1].id = 0; }), /duplicate group id 0/);
  assert.match(broken((m) => { m.steps[0].groups[2].children = [9]; }), /G2: children and parents must name groups/);
  assert.match(broken((m) => { m.steps[0].groups[2].queries = [1]; }), /must name workload queries/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidates[0].origin = 'magic'; }), /unknown origin "magic"/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidates[1].index = 0; }), /index must be a unique candidate index/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidate_count = 2; }), /candidate_count must count the candidates/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidate_count_by_origin = { pass1: 6 }; }), /candidate_count_by_origin/);
  assert.match(broken((m) => { delete m.steps[1].groups[3].candidates[0].label; }), /needs a label and an operator/);
  assert.match(broken((m) => { delete m.steps[1].groups[3].operator; }), /G3: needs a label and an operator/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidates[1].fused_groups = [9]; }), /children and fused_groups must name groups/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidates[1].parameters = 7; }), /parameters must be an object or null/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidates[1].required_accuracy = []; }), /required_accuracy must hold one/);
  assert.match(broken((m) => { delete m.steps[PASS2].groups[3].candidates[0].pruned; }), /pruned must be a boolean/);
  // Pass 2 lists the logical edge from every candidate of the input group.
  assert.match(broken((m) => { m.steps[PASS2].groups[4].candidates[0].inputs[0].edges.pop(); }), /inputs must list, per input group/);
  assert.match(broken((m) => { m.steps[PASS2].groups[4].candidates[0].inputs[0].edges[0].producer = 8; }), /inputs must list, per input group/);
  assert.match(broken((m) => { m.selected_candidates = { 9: [0] }; }), /9 is not a group of the last step/);
  assert.match(broken((m) => { m.selected_candidates = { 3: [8] }; }), /G3 must list candidate indices of the group/);
  assert.match(broken((m) => { m.selected_candidates = { 3: 1 }; }), /G3 must list candidate indices of the group/);
  // A document without a selected plan has no selected_candidates.
  assert.equal(broken((m) => { delete m.selected_candidates; }), '');
});

test('each step says what it added over the previous one', () => {
  const memo = fixture();
  assert.deepEqual(call('memoStepDiff', memo, 0), { groups: 3, groupsAdded: [0, 1, 2], groupsRemoved: [], candidates: 0, candidatesDelta: 0, added: 0 });
  const pass1 = call('memoStepDiff', memo, 1);
  assert.equal(pass1.candidates, 12);
  assert.deepEqual(pass1.groupsAdded, [3, 4]);
  // Pass 2 adds the tumbling-window chain G5, its range G6 and a merge in G3.
  const pass2 = call('memoStepDiff', memo, PASS2);
  assert.deepEqual(pass2.groupsAdded, [5, 6]);
  assert.equal(pass2.candidatesDelta, 1 + 3 + 1);
  assert.equal(pass2.added, 5);
  assert.equal(call('memoStepText', memo, 0), '3 groups · 0 candidates');
  assert.equal(call('memoStepText', memo, PASS2), '7 groups (+2) · 17 candidates (+5) · 5 candidates new at this step');
});

test('stage 0 draws one box per group and an edge per child group', () => {
  const elements = call('memoStepElements', fixture(), 0, { queryIds: ['q1'] });
  assert.equal(nodesOf(elements).filter((e) => e.data.kind === 'group').length, 3);
  assert.deepEqual(edgesOf(elements).map((e) => e.data.id).sort(), ['g1>g0', 'g2>g1']);
  assert.match(byId(elements, 'g2').data.label, /^G2 · Aggregate quantile\(q=0\.9\)\nno candidates yet · root of q1$/);
  // Consumers sit above what they read.
  assert.ok(byId(elements, 'g2').position.y < byId(elements, 'g1').position.y);
  assert.equal(cytoscape(elements).nodes().length, 3);
});

test('each candidate sits inside its group, colored by origin, with θ, and an edge to each child group it reads', () => {
  const elements = call('memoStepElements', fixture(), PASS2, {});
  const kll = byId(elements, 'g3c5');
  assert.equal(kll.data.parent, 'g3');
  assert.equal(kll.data.color, vm.runInContext('MEMO_ORIGINS.pass1.color', context));
  assert.equal(kll.data.label, '#5 SummaryAgg Kll\nθ Kll(k=1404)\nPass1 · ✓ selected');
  // A candidate without θ says so on its middle line; a new one is marked.
  assert.equal(byId(elements, 'g3c7').data.label, '#7 SummaryMerge\n—\nTumblingWindows · ✚ new');
  assert.match(byId(elements, 'g3c7').classes, /added/);
  assert.equal(call('memoClip', 'abcdef', 4), 'abc…');
  assert.ok(byId(elements, 'g3c5>g1'));
  assert.ok(byId(elements, 'g3c7>g6'));
  const cy = cytoscape(elements);
  assert.equal(cy.getElementById('g3').children().length, 8);
  assert.equal(cy.edges().length, edgesOf(elements).length);
});

test("Pass 2's pruned candidates are marked and counted on their group", () => {
  const elements = call('memoStepElements', fixture(), PASS2, {});
  assert.deepEqual(nodesOf(elements).filter((e) => /\bpruned\b/.test(e.classes || '')).map((e) => e.data.id), ['g3c0', 'g3c1', 'g3c2', 'g3c3']);
  assert.match(byId(elements, 'g3c0').data.label, /Pass1 · ✗ pruned$/);
  assert.match(byId(elements, 'g3').data.label, /^G3 · SummaryAgg Kll\n8 candidates \(4 pruned by Pass 2\)$/);
  // Pass 1 does not prune.
  assert.ok(!call('memoStepElements', fixture(), 1, {}).some((e) => /pruned/.test(e.classes || '')));
});

test('a large group collapses to one count per origin and expands on request', () => {
  const memo = fixture();
  const inG3 = (elements) => nodesOf(elements).filter((e) => e.data.parent === 'g3');
  const collapsed = call('memoStepElements', memo, PASS2, { collapseAbove: 4 });
  assert.deepEqual(inG3(collapsed).map((e) => e.data.label), ['7 Pass1\n✗ 4 pruned · ✓ selected', '1 TumblingWindows\n✚ 1 new']);
  assert.match(byId(collapsed, 'g3').data.label, /click a count to expand/);
  assert.match(byId(collapsed, 'g3opass1').classes, /chosen/);
  assert.ok(byId(collapsed, 'g3otumbling_windows>g6'));
  const open = call('memoStepElements', memo, PASS2, { collapseAbove: 4, expanded: [3] });
  assert.equal(inG3(open).length, 8);
  assert.match(byId(open, 'g3').data.label, /click the group to collapse/);
  assert.equal(cytoscape(open).getElementById('g3').children().length, 8);
});

test('the selected candidates are highlighted on the last step only', () => {
  const memo = fixture();
  const last = call('memoStepElements', memo, PASS2, {});
  const chosen = nodesOf(last).filter((e) => /\bchosen\b/.test(e.classes)).map((e) => e.data.id).sort();
  assert.deepEqual(chosen, ['g0c0', 'g1c0', 'g2c1', 'g3c5', 'g4c0']);
  assert.match(byId(last, 'g3c5>g1').classes, /chosen/);
  assert.ok(!call('memoStepElements', memo, 1, {}).some((e) => /chosen/.test(e.classes || '')));
  delete memo.selected_candidates;
  assert.ok(!call('memoStepElements', memo, PASS2, {}).some((e) => /chosen/.test(e.classes || '')));
});

test('a collapsed count marks only the selected candidate\'s edges', () => {
  const memo = fixture();
  // Another Pass1 candidate of G3 reads G6 instead of G1.
  memo.steps[PASS2].groups[3].candidates[6].children = [6];
  const elements = call('memoStepElements', memo, PASS2, { collapseAbove: 4 });
  assert.match(byId(elements, 'g3opass1>g1').classes, /chosen/);
  assert.equal(byId(elements, 'g3opass1>g6').classes, '');
});

test('highlighting a query dims the groups it does not reach', () => {
  const memo = fixture();
  memo.steps[PASS2].groups[6].queries = [];
  const elements = call('memoStepElements', memo, PASS2, {});
  const dimmed = new Set(call('memoQueryDimmedIds', elements, 0));
  assert.ok(dimmed.has('g6') && dimmed.has('g6c0') && dimmed.has('g3c7>g6'));
  assert.ok(!dimmed.has('g3') && !dimmed.has('g3c5') && !dimmed.has('g3c5>g1'));
});

test("a candidate's θ and required accuracy per input", () => {
  const groups = fixture().steps[PASS2].groups;
  assert.equal(call('memoParametersText', groups[3].candidates[5].parameters), 'θ Kll(k=1404)');
  assert.equal(call('memoParametersText', null), null);
  assert.deepEqual(call('memoRequiredAccuracyTexts', groups[4].candidates[0]), ['G3: rank ≤ 0.01, δ 0.00001']);
  assert.deepEqual(call('memoRequiredAccuracyTexts', groups[3].candidates[5]), ['G1: none']);
});

test("Pass 2's logical edges into a candidate: each producer of the input group, with α^log and why it is 0", () => {
  const estimate = fixture().steps[PASS2].groups[4].candidates[0];
  const [input] = call('memoLogicalEdges', estimate);
  assert.equal(input.title, 'input G3 · required rank ≤ 0.01, δ 0.00001');
  assert.equal(input.edges.length, 8);
  assert.match(input.edges[0], /^✗ #0: α\^log = 0: the producer's guarantee exceeds Rank ≤ 0\.01/);
  assert.equal(input.edges[5], '✓ #5');
});

test("a candidate's physical candidates list each input's physical edges with α^phys, the selected one marked", () => {
  const doc = fixtureDoc();
  const physical = call('memoPhysicalCandidates', doc, 4, 0);
  assert.deepEqual(physical.map((p) => p.title), ['C18 (query_time, None)', 'C19 (query_time, OneEvaluation(Memory))']);
  // Only edges whose logical edge holds: G3's four kept candidates' physical candidates.
  assert.deepEqual(physical[1].inputs, [{ title: 'input G3', edges: ['✓ C10', '✓ C11', '✓ C12 · selected', '✓ C13', '✓ C14', '✓ C15', '✓ C16', '✓ C17'] }]);
  const rejected = doc.stage2_physical_asap_optimization.physical_candidates.find((c) => c.inputs.some((i) => i.edges.some((e) => e.rejection)));
  const lines = call('memoPhysicalCandidates', doc, rejected.group, rejected.index_in_group).flatMap((p) => p.inputs.flatMap((i) => i.edges));
  assert.ok(lines.some((line) => /^✗ C\d+: α\^phys = 0: consumer \(/.test(line)), lines.join('\n'));
  // Pruned by Pass 2: Stage 2 made no physical candidate of it.
  assert.deepEqual(call('memoPhysicalCandidates', doc, 3, 0), []);
});

test('the pruned candidates of a group: Pass 2 with its root causes, Stage 2 and Stage 3 with their reasons', () => {
  const doc = fixtureDoc();
  const pruned = call('memoPrunedLines', doc, 3);
  assert.equal(pruned.pass2.length, 4);
  assert.match(pruned.pass2[0], /^#0: the producer's guarantee exceeds Rank ≤ 0\.01 \(δ 0\.00001\)$/);
  assert.ok(pruned.stage2.includes('#4 (query_time, AcrossEvaluations(Memory)): query time results not retained'), pruned.stage2.join('\n'));
  assert.deepEqual(pruned.stage3, []);
  // A cause traced through eliminated consumers names them; a Stage 3
  // reason names the physical candidate that dominates it.
  doc.stage1_logical_asap_optimization.pruned_candidates[0].causes[0].through_pruned_candidates = [{ group: 4, index_in_group: 0 }, { group: 2, index_in_group: 1 }];
  doc.stage3_plan_selection.pruned_candidates = [
    { group: 3, index_in_group: 6, properties: { timing: 'query_time', lifetime: 'None' }, reason: { dominated_by: 12 } },
    { group: 3, index_in_group: 7, properties: { timing: 'query_time', lifetime: 'None' }, reason: 'usage_not_costable' }];
  const traced = call('memoPrunedLines', doc, 3);
  assert.match(traced.pass2[0], /^#0: through G4\[0\] → G2\[1\]: the producer's guarantee exceeds/);
  assert.deepEqual(call('memoPass2Causes', doc, 3, 1).length, 1);
  assert.deepEqual(call('memoPass2Causes', doc, 3, 5), []);
  assert.deepEqual(traced.stage3, ['#6 (query_time, None): dominated by C12', '#7 (query_time, None): usage not costable']);
  assert.deepEqual(call('memoPrunedLines', { memo: doc.memo }, 3), { pass2: [], stage2: [], stage3: [] });
});
