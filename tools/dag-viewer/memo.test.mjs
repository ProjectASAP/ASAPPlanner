// Node tests of memo.js (no DOM): the `memo` section checks, the per-step
// diff, a step's elements (groups holding their candidates, collapsed large
// groups, the selected candidates, query dimming), a candidate's θ, ω and
// input parameter floors, its physical candidates' edges and the pruned
// candidates of Stages 2 and 3, over the hand-written fixture
// test-fixtures/memo-small.json (a `memo` section with the Stage 2 and
// Stage 3 sections the memo view reads); the elements
// load into the vendored cytoscape (headless). Run from the repository root:
// `node --test tools/dag-viewer/memo.test.mjs`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { planned, planner } from './test-fixtures/planned.mjs';

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
const fixtureDoc = () => JSON.parse(readFileSync(new URL('test-fixtures/memo-small.json', here), 'utf8'));
const fixture = () => fixtureDoc().memo;
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
  assert.match(broken((m) => { m.steps[1].groups[2].candidate_count_by_origin = { pass1: 1, tumbling_windows: 1 }; m.steps[1].groups[2].candidates[1].origin = 'rollup'; }), /origin rollup is not counted/);
  assert.match(broken((m) => { delete m.steps[1].groups[3].candidates[0].label; }), /needs a label and an operator/);
  assert.match(broken((m) => { delete m.steps[1].groups[3].operator; }), /G3: needs a label and an operator/);
  assert.match(broken((m) => { m.steps[1].groups[5].candidates[1].fused_groups = [9]; }), /children and fused_groups must name groups/);
  assert.match(broken((m) => { m.steps[1].groups[5].candidates[1].logical_properties = { parameters: 7, window_layout: null }; }), /logical_properties needs parameters and window_layout/);
  assert.match(broken((m) => { m.steps[1].groups[5].candidates[1].input_requirements = []; }), /one parameter_floor_for_required_accuracy per input/);
  assert.match(broken((m) => { m.selected_candidates = { 9: [0] }; }), /9 is not a group of the last step/);
  assert.match(broken((m) => { m.selected_candidates = { 5: [3] }; }), /G5 must list candidate indices of the group/);
  assert.match(broken((m) => { m.selected_candidates = { 5: 1 }; }), /G5 must list candidate indices of the group/);
  // A document without a selected plan has no selected_candidates.
  assert.equal(broken((m) => { delete m.selected_candidates; }), '');
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
  // G2 grows from 2 to 20 (18 TumblingWindows and SummaryInput, 11 listed), G3 gains a
  // rollup, the new tumbling-window-chain group G6 has one.
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
  // Consumers sit above what they read.
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
  assert.equal(cms.data.label, '#1 CMS+heap depth 7 width 272\nθ CmsWithHeap(depth=7, heap_s…\nPass1 · ✚ new');
  // A candidate without θ or ω says so on its middle line.
  assert.equal(byId(elements, 'g5c0').data.label.split('\n')[1], '—');
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
  assert.deepEqual(inG2(collapsed).map((e) => e.data.label.split('\n')[0]), ['2 Pass1', '9 TumblingWindows', '9 SummaryInput']);
  assert.match(byId(collapsed, 'g2').data.label, /20 candidates \(13 listed\) · click a count to expand/);
  // The selected candidates (G2 #2, a TumblingWindows candidate, and #8, a
  // SummaryInput one) mark their counts.
  assert.match(byId(collapsed, 'g2otumbling_windows').classes, /chosen/);
  assert.match(byId(collapsed, 'g2otumbling_windows').data.label, /✚ 6 new · ✓ selected/);
  assert.ok(byId(collapsed, 'g2otumbling_windows>g6'));
  assert.match(byId(collapsed, 'g2osummary_input').classes, /chosen/);
  assert.ok(byId(collapsed, 'g2opass1>g1'));
  const open = call('memoStepElements', memo, 2, { expanded: [2] });
  assert.equal(inG2(open).length, 13);
  assert.match(byId(open, 'g2').data.label, /click the group to collapse/);
  assert.match(byId(open, 'g2c2').classes, /chosen/);
  assert.match(byId(open, 'g2c2>g6').classes, /chosen/);
  assert.equal(cytoscape(open).getElementById('g2').children().length, 13);
  // An origin with no listed candidate says so.
  const capped = fixture();
  capped.steps[2].groups[2].candidates = capped.steps[2].groups[2].candidates.filter((c) => c.origin !== 'summary_input');
  assert.equal(byId(call('memoStepElements', capped, 2, { collapseAbove: 4 }), 'g2osummary_input').data.label, '9 SummaryInput\nnone listed');
  // A threshold above the listed count draws every candidate.
  assert.equal(inG2(call('memoStepElements', memo, 2, { collapseAbove: 20 })).length, 13);
});

test('the selected candidates, several per group, are highlighted on the last step only', () => {
  const memo = fixture();
  const last = call('memoStepElements', memo, 2, { expanded: [2] });
  const chosen = nodesOf(last).filter((e) => /\bchosen\b/.test(e.classes)).map((e) => e.data.id).sort();
  assert.deepEqual(chosen, ['g0c0', 'g1c0', 'g2c2', 'g2c8', 'g3c0', 'g4c0', 'g5c1', 'g6c0']);
  assert.match(byId(last, 'g5c1').data.label, /✓ selected/);
  delete memo.selected_candidates;
  assert.ok(!call('memoStepElements', memo, 2, {}).some((e) => /chosen/.test(e.classes || '')));
});

test('a collapsed count marks only the selected candidate\'s edges', () => {
  const memo = fixture();
  // Another listed TumblingWindows candidate of G2 reads G1 instead of G6.
  const other = memo.steps[2].groups[2].candidates.find((c) => c.index === 3);
  other.children = [1];
  const elements = call('memoStepElements', memo, 2, {});
  assert.match(byId(elements, 'g2otumbling_windows>g6').classes, /chosen/);
  assert.equal(byId(elements, 'g2otumbling_windows>g1').classes, '');
});

test('a selected candidate past the listed prefix is named on its group', () => {
  const memo = fixture();
  memo.selected_candidates['2'] = [15];
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

test("a candidate's θ, ω and per-input parameter floors read its logical properties and input requirements", () => {
  const memo = fixture();
  const cms = memo.steps[2].groups[5].candidates[1];
  assert.equal(call('memoParametersText', cms.logical_properties.parameters), 'θ CmsWithHeap(depth=7, heap_size=10, width=272)');
  assert.equal(call('memoParametersText', null), null);
  const chain = memo.steps[2].groups[6].candidates[0];
  assert.equal(call('memoWindowLayoutText', chain.logical_properties.window_layout), 'ω tumbling 10 s');
  assert.equal(call('memoWindowLayoutText', null), null);
  // One floor per input group, "none" for an input without one.
  const estimate = { children: [3, 4], input_requirements: [
    { parameter_floor_for_required_accuracy: { Kll: { k: 269 } } }, { parameter_floor_for_required_accuracy: null }] };
  assert.deepEqual(call('memoFloorTexts', estimate), ['G3: Kll(k=269)', 'G4: none']);
});

test("a candidate's physical candidates list each input's producer edges, admissible or with their rejection", () => {
  const doc = fixtureDoc();
  const physical = call('memoPhysicalCandidates', doc, 5, 1);
  assert.deepEqual(physical.map((p) => p.title), ['C2 (query_time, OneEvaluation(Memory))', 'C3 (ingestion_time, AcrossEvaluations(Memory))']);
  assert.deepEqual(physical[0].inputs, [{ title: 'input G1 · floor none', edges: [
    '✗ C1: α^phys = 0: consumer (QueryTime, OneEvaluation(Memory)) cannot read producer (IngestionTime, None)'] }]);
  assert.equal(physical[0].pruned, 'pruned by Stage 3: dominated_by (dominated by C3)');
  // An edge of the selected DAG is marked.
  assert.deepEqual(physical[1].inputs[0].edges, ['✓ C1 · selected']);
  assert.equal(physical[1].pruned, null);
  assert.deepEqual(call('memoPhysicalCandidates', doc, 5, 0), []);
});

test('the pruned candidates of a group: Stage 2 by memo candidate and properties, Stage 3 by physical candidate and what dominates it', () => {
  const doc = fixtureDoc();
  assert.deepEqual(call('memoPrunedLines', doc, 5), {
    stage2: ['G5[2] (ingestion_time, None): no_ingestion_pipeline'],
    stage3: ['C2: dominated_by (dominated by C3)'],
  });
  assert.deepEqual(call('memoPrunedLines', doc, 1), { stage2: [], stage3: ['C4: usage_not_costable'] });
  assert.deepEqual(call('memoPrunedLines', { memo: doc.memo }, 5), { stage2: [], stage3: [] });
});

// The memo asap_planner writes for #1005's ratio-of-quantiles query
// (skipped when asap_planner is not built).
test('the real example: each step draws its groups and counts its new candidates', { skip: !planner }, () => {
  const doc = planned('memo-examples/promql/ratio-of-quantiles-5m.json');
  const memo = doc.memo;
  assert.deepEqual(call('validateMemo', memo, doc.workload.queries.length), []);
  const steps = memo.steps.map((s) => s.step);
  const groupsOf = (step) => memo.steps[steps.indexOf(step)].groups;
  const stage0 = call('memoStepElements', memo, steps.indexOf('stage0'), {});
  assert.equal(nodesOf(stage0).filter((e) => e.data.kind === 'group').length, groupsOf('stage0').length);
  const pass1 = groupsOf('pass1').reduce((n, g) => n + g.candidates.length, 0);
  assert.equal(call('memoStepDiff', memo, steps.indexOf('pass1')).candidates, pass1);
  const last = memo.steps.length - 1;
  const elements = call('memoStepElements', memo, last, {});
  assert.equal(nodesOf(elements).filter((e) => e.data.kind === 'group').length, memo.steps[last].groups.length);
  assert.ok(cytoscape(elements).nodes().length >= memo.steps[last].groups.length);
  // A candidate is one operator over its input groups, one input
  // requirement each.
  memo.steps.forEach((step) => step.groups.forEach((g) => g.candidates.forEach((c) => {
    assert.deepEqual(call('stageLogicalChildren', call('stageOperatorParts', c.operator)[1]), c.children);
    assert.equal(c.input_requirements.length, c.children.length);
  })));
  // Some quantile estimate reads a sketch with a parameter floor; some
  // candidate has θ and ω.
  const lastGroups = memo.steps[last].groups;
  assert.ok(lastGroups.some((g) => g.candidates.some((c) => call('memoFloorTexts', c).some((f) => /Kll\(k=\d+\)/.test(f)))));
  assert.ok(lastGroups.some((g) => g.candidates.some((c) => c.logical_properties.parameters && c.logical_properties.window_layout)));
  // Every selected candidate has a physical candidate with a selected edge
  // into each of its inputs, and the stage sections pass their checks.
  assert.deepEqual(call('validatePrunedAndPhysical', doc.stage2_physical_asap_optimization, doc.stage3_plan_selection), []);
  Object.entries(memo.selected_candidates).forEach(([group, indices]) => indices.forEach((index) => {
    const physical = call('memoPhysicalCandidates', doc, Number(group), index);
    assert.ok(physical.some((p) => p.inputs.every((input) => input.edges.some((e) => /^✓ C\d+ · selected$/.test(e)))), `G${group}[${index}]`);
  }));
});
