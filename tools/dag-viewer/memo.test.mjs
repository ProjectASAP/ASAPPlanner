// Node tests of memo.js (no DOM): the `memo` section checks, the per-step
// diff, a step's elements (groups holding their candidates, collapsed large
// groups, the selected choice, query dimming) and a candidate's own nodes,
// over the hand-written fixture test-fixtures/memo-small.json; the elements
// load into the vendored cytoscape (headless). Run from the repository root:
// `node --test tools/dag-viewer/memo.test.mjs`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
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
const fixture = () => JSON.parse(readFileSync(new URL('test-fixtures/memo-small.json', here), 'utf8')).memo;
const nodesOf = (elements) => elements.filter((e) => !('source' in e.data));
const edgesOf = (elements) => elements.filter((e) => 'source' in e.data);
const byId = (elements, id) => elements.find((e) => e.data.id === id);

test('the fixture is a valid memo; a document without one has nothing to check', () => {
  assert.deepEqual(call('validateMemo', fixture(), 2), []);
  assert.deepEqual(call('validateMemo', undefined, 2), []);
});

test('memo violations are reported', () => {
  const broken = (edit) => { const memo = fixture(); edit(memo); return call('validateMemo', memo, 2).join('\n'); };
  assert.match(broken((m) => { m.steps = []; }), /memo.steps must be a non-empty array/);
  assert.match(broken((m) => { m.steps[1].step = 'stage0'; }), /out of pipeline order or repeated/);
  assert.match(broken((m) => { m.steps[0].step = 'pass3'; }), /step must be one of/);
  assert.match(broken((m) => { m.steps[0].groups[1].id = 0; }), /duplicate group id 0/);
  assert.match(broken((m) => { m.steps[0].groups[2].children = [9]; }), /G2: children and parents must name groups/);
  assert.match(broken((m) => { m.steps[0].groups[2].queries = [2]; }), /must name workload queries/);
  assert.match(broken((m) => { m.steps[1].groups[5].candidates[0].origin = 'magic'; }), /unknown origin "magic"/);
  assert.match(broken((m) => { m.steps[1].groups[5].candidates[1].index = 0; }), /index must be a unique candidate index/);
  assert.match(broken((m) => { m.steps[1].groups[5].candidate_count = 2; }), /candidate_count must count at least/);
  assert.match(broken((m) => { m.steps[2].groups[2].candidates_capped = false; }), /candidates_capped/);
  assert.match(broken((m) => { m.steps[1].groups[2].candidate_count_by_origin = { pass1: 1 }; }), /candidate_count_by_origin/);
  assert.match(broken((m) => { m.steps[1].groups[2].candidate_count_by_origin = { pass1: 1, panes: 1 }; m.steps[1].groups[2].candidates[1].origin = 'rollup'; }), /origin rollup is not counted/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidates[0].group_of_node = { '1.5': 2 }; }), /group_of_node must map/);
  assert.match(broken((m) => { delete m.steps[1].groups[3].candidates[0].label; }), /needs a label/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidates[0].group_of_node = { 0: 4 }; }), /group_of_node must map/);
  assert.match(broken((m) => { m.steps[1].groups[3].candidates[0].dag.nodes[3].operator.NonASAP.Aggregate.child = 7; }), /reads missing node 7/);
  assert.match(broken((m) => { m.selected_choice = { 9: 0 }; }), /9 is not a group of the last step/);
  assert.match(broken((m) => { m.selected_choice = { 5: 3 }; }), /G5 has no candidate 3/);
  assert.match(broken((m) => { m.selected_choice = null; }), /without selected_choice_unavailable/);
  assert.equal(broken((m) => { m.selected_choice = null; m.selected_choice_unavailable = 'no plan selected'; }), '');
});

test('each step says what it added over the previous one', () => {
  const memo = fixture();
  assert.deepEqual(call('memoStepDiff', memo, 0), { groups: 6, groupsAdded: [0, 1, 2, 3, 4, 5], groupsRemoved: [], candidates: 0, candidatesDelta: 0, listedAdded: 0 });
  const pass1 = call('memoStepDiff', memo, 1);
  assert.equal(pass1.candidates, 10);
  assert.equal(pass1.candidatesDelta, 10);
  assert.equal(pass1.listedAdded, 10);
  assert.deepEqual(pass1.groupsAdded, []);
  const pass2 = call('memoStepDiff', memo, 2);
  // G2 grows from 2 to 20 (18 Panes and SharedBuild, 11 listed), G3 gains a
  // rollup, the new pane-chain group G6 has one.
  assert.deepEqual(pass2.groupsAdded, [6]);
  assert.equal(pass2.candidatesDelta, 18 + 1 + 1);
  assert.equal(pass2.listedAdded, 11 + 1 + 1);
  assert.equal(call('memoStepText', memo, 0), '6 groups · 0 candidates');
  assert.equal(call('memoStepText', memo, 2), '7 groups (+1) · 30 candidates (+20) · 13 listed candidates new at this step');
});

test('stage 0 draws one box per group and an edge per child group', () => {
  const elements = call('memoStepElements', fixture(), 0, { queryIds: ['Q1', 'Q2'] });
  const groups = nodesOf(elements).filter((e) => e.data.kind === 'group');
  assert.equal(groups.length, 6);
  assert.equal(nodesOf(elements).length, 6);
  assert.deepEqual(edgesOf(elements).map((e) => e.data.id).sort(), ['g1>g0', 'g2>g1', 'g3>g2', 'g4>g1', 'g5>g4']);
  assert.match(byId(elements, 'g3').data.label, /^G3 · sum by \(job\)\nno candidates yet · root of Q1$/);
  // Readers sit above what they read.
  assert.ok(byId(elements, 'g3').position.y < byId(elements, 'g2').position.y);
  assert.ok(byId(elements, 'g2').position.y < byId(elements, 'g1').position.y);
  assert.equal(cytoscape(elements).nodes().length, 6);
});

test('pass 1 puts each candidate inside its group, colored by origin, with an edge to each child group it reads', () => {
  const elements = call('memoStepElements', fixture(), 1, {});
  const candidates = nodesOf(elements).filter((e) => e.data.kind === 'candidate');
  assert.equal(candidates.length, 10);
  const cms = byId(elements, 'g5c1');
  assert.equal(cms.data.parent, 'g5');
  assert.equal(cms.data.color, vm.runInContext('MEMO_ORIGINS.pass1.color', context));
  assert.match(cms.data.classes || cms.classes, /added/);
  assert.equal(cms.data.label, '#1 CMS+heap depth 7 width 272\nPass1 · ✚ new');
  assert.equal(call('memoClip', 'abcdef', 4), 'abc…');
  // The exact topk reads G4, the sketches read the range G1 directly.
  assert.ok(byId(elements, 'g5c0>g4'));
  assert.ok(byId(elements, 'g5c1>g1'));
  // Not the last step: no selection.
  assert.ok(!elements.some((e) => /chosen/.test(e.classes || '')));
  const cy = cytoscape(elements);
  assert.equal(cy.getElementById('g5').children().length, 3);
  assert.equal(cy.edges().length, edgesOf(elements).length);
});

test('a large group collapses to one count per origin and expands on request', () => {
  const memo = fixture();
  const collapsed = call('memoStepElements', memo, 2, {});
  const inG2 = (elements) => nodesOf(elements).filter((e) => e.data.parent === 'g2');
  assert.deepEqual(inG2(collapsed).map((e) => e.data.label.split('\n')[0]), ['2 Pass1', '9 Panes', '9 SharedBuild']);
  assert.match(byId(collapsed, 'g2').data.label, /20 candidates \(13 listed\) · click a count to expand/);
  // The selected candidate (G2 #2, a Panes candidate) marks its count.
  assert.match(byId(collapsed, 'g2opanes').classes, /chosen/);
  assert.match(byId(collapsed, 'g2opanes').data.label, /✚ 6 new · ✓ selected/);
  assert.ok(byId(collapsed, 'g2opanes>g6'));
  assert.ok(byId(collapsed, 'g2opass1>g1'));
  const open = call('memoStepElements', memo, 2, { expanded: [2] });
  assert.equal(inG2(open).length, 13);
  assert.match(byId(open, 'g2').data.label, /click the group to collapse/);
  assert.match(byId(open, 'g2c2').classes, /chosen/);
  assert.match(byId(open, 'g2c2>g6').classes, /chosen/);
  assert.equal(cytoscape(open).getElementById('g2').children().length, 13);
  // An origin with no listed candidate says so.
  const capped = fixture();
  capped.steps[2].groups[2].candidates = capped.steps[2].groups[2].candidates.filter((c) => c.origin !== 'shared_build');
  assert.equal(byId(call('memoStepElements', capped, 2, { collapseAbove: 4 }), 'g2oshared_build').data.label, '9 SharedBuild\nnone listed');
  // A threshold above the listed count draws every candidate.
  assert.equal(inG2(call('memoStepElements', memo, 2, { collapseAbove: 20 })).length, 13);
});

test('the selected choice is highlighted on the last step only', () => {
  const memo = fixture();
  const last = call('memoStepElements', memo, 2, { expanded: [2] });
  const chosen = nodesOf(last).filter((e) => /\bchosen\b/.test(e.classes)).map((e) => e.data.id).sort();
  assert.deepEqual(chosen, ['g0c0', 'g1c0', 'g2c2', 'g3c0', 'g4c0', 'g5c1', 'g6c0']);
  assert.match(byId(last, 'g5c1').data.label, /✓ selected/);
  memo.selected_choice = null;
  memo.selected_choice_unavailable = 'no plan selected';
  assert.ok(!call('memoStepElements', memo, 2, {}).some((e) => /chosen/.test(e.classes || '')));
});

test('a collapsed count marks only the selected candidate\'s edges', () => {
  const memo = fixture();
  // Another listed Panes candidate of G2 reads G1 instead of G6.
  const other = memo.steps[2].groups[2].candidates.find((c) => c.index === 3);
  other.children = [1];
  other.group_of_node = { 1: 1 };
  const elements = call('memoStepElements', memo, 2, {});
  assert.match(byId(elements, 'g2opanes>g6').classes, /chosen/);
  assert.equal(byId(elements, 'g2opanes>g1').classes, '');
});

test('a selected candidate past the listed prefix is named on its group', () => {
  const memo = fixture();
  memo.selected_choice['2'] = 15;
  assert.deepEqual(call('validateMemo', memo, 2), []);
  const elements = call('memoStepElements', memo, 2, {});
  assert.match(byId(elements, 'g2').data.label, /✓ selected #15 \(not listed\)/);
  assert.ok(!nodesOf(elements).some((e) => e.data.parent === 'g2' && /chosen/.test(e.classes)));
});

test('highlighting a query dims the groups it does not reach', () => {
  const elements = call('memoStepElements', fixture(), 1, {});
  const dimmed = new Set(call('memoQueryDimmedIds', elements, 1));
  // Q2 (index 1) reaches G0, G1, G4 and G5; G2 and G3 are Q1's only.
  assert.ok(dimmed.has('g2') && dimmed.has('g3') && dimmed.has('g2c0') && dimmed.has('g3c1>g2'));
  assert.ok(!dimmed.has('g5') && !dimmed.has('g5c1') && !dimmed.has('g1') && !dimmed.has('g5c1>g1'));
  // An edge from a reached group into a dimmed one is dimmed too.
  assert.ok(dimmed.has('g2c0>g1'));
});

test("a candidate's own nodes stop at its child groups' nodes", () => {
  const memo = fixture();
  const accumulator = memo.steps[1].groups[3].candidates[1];
  const { own, reads } = call('memoCandidateOwnNodes', accumulator);
  assert.deepEqual(own.map((n) => n.lines[0]), ['FinalizeExactAccumulator', 'SummaryAgg']);
  assert.deepEqual(reads, [{ node: 3, group: 2 }]);
  const cms = call('memoCandidateOwnNodes', memo.steps[1].groups[5].candidates[1]);
  assert.deepEqual(cms.own.map((n) => n.lines[0]), ['Project', 'SummaryEstimate', 'SummaryAgg']);
  assert.deepEqual(cms.reads.map((r) => r.group), [1]);
  // A group with no child group reads nothing: its whole dag is its own.
  assert.deepEqual(call('memoCandidateOwnNodes', memo.steps[1].groups[0].candidates[0]).own.map((n) => n.lines[0]), ['Scan']);
});

// The planner's real memo for the issue's example (#1003 writes it).
const REAL = new URL('examples/memo-ratio-of-quantiles.json', here);
test('the real example: 8 groups at Stage 0, 26 candidates after Pass 1, G2 Panes and SharedBuild later', { skip: !existsSync(REAL) }, () => {
  const doc = JSON.parse(readFileSync(REAL, 'utf8'));
  const memo = doc.memo;
  assert.deepEqual(call('validateMemo', memo, doc.workload.queries.length), []);
  const steps = memo.steps.map((s) => s.step);
  const stage0 = call('memoStepElements', memo, steps.indexOf('stage0'), {});
  assert.equal(nodesOf(stage0).filter((e) => e.data.kind === 'group').length, 8);
  assert.equal(call('memoStepDiff', memo, steps.indexOf('pass1')).candidates, 26);
  const later = memo.steps.length - 1;
  const g2 = call('memoStepElements', memo, later, {}).filter((e) => e.data.parent === 'g2').map((e) => e.data.label.split('\n')[0]);
  const byOrigin = memo.steps[later].groups.find((g) => g.id === 2).candidate_count_by_origin;
  assert.ok(byOrigin.panes > 0 && byOrigin.shared_build > 0);
  assert.ok(g2.includes(`${byOrigin.panes} Panes`) && g2.includes(`${byOrigin.shared_build} SharedBuild`));
  cytoscape(call('memoStepElements', memo, later, {}));
});
