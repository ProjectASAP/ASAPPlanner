// Node tests of memo.js (no DOM): the `memo` section checks, the per-step
// diff, a step's elements (groups holding their candidates, collapsed large
// groups, the selected and pruned candidates, query dimming), a candidate's
// θ and required accuracy, its logical edges, its physical candidates'
// edges and the pruned candidates of Pass 2, Stage 2 and Stage 3, over
// test-fixtures/p90-kll.json: the document asap_planner writes for
// test-fixtures/p90-workload.json under test-fixtures/kll-only-no-raw-data-capabilities.json
// (one p90 query with accuracy enforced, KLL only and no raw data kept, so
// the plan reads a KLL tumbling-window chain; Pass 2 prunes G4's four
// smallest KLL sketches). Group and candidate ids are the document's
// canonical ones: G0 is the query's root, G1 its range, G2 the scan, G3
// the readout, G4 the KLL states, G5 the chain's range and G6 the chain.
// The elements load into the vendored cytoscape (headless).
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
  assert.match(broken((m) => { m.steps[1].groups[4].candidates[0].origin = 'magic'; }), /unknown origin "magic"/);
  assert.match(broken((m) => { m.steps[1].groups[4].candidates[1].index = 0; }), /index must be a unique candidate index/);
  assert.match(broken((m) => { m.steps[1].groups[4].candidate_count = 2; }), /candidate_count must count the candidates/);
  assert.match(broken((m) => { m.steps[1].groups[4].candidate_count_by_origin = { pass1: 6 }; }), /candidate_count_by_origin/);
  assert.match(broken((m) => { delete m.steps[1].groups[4].candidates[0].label; }), /needs a label and an operator/);
  assert.match(broken((m) => { delete m.steps[1].groups[4].operator; }), /G4: needs a label and an operator/);
  assert.match(broken((m) => { m.steps[1].groups[4].candidates[1].fused_groups = [9]; }), /children and fused_groups must name groups/);
  assert.match(broken((m) => { m.steps[1].groups[4].candidates[1].parameters = 7; }), /parameters must be an object or null/);
  assert.match(broken((m) => { m.steps[1].groups[4].candidates[1].required_accuracy = []; }), /required_accuracy must hold one/);
  assert.match(broken((m) => { delete m.steps[PASS2].groups[3].candidates[0].pruned; }), /pruned must be a boolean/);
  // Pass 2 lists the logical edge from every candidate of the input group.
  assert.match(broken((m) => { m.steps[PASS2].groups[3].candidates[0].inputs[0].edges.pop(); }), /inputs must list, per input group/);
  assert.match(broken((m) => { m.steps[PASS2].groups[3].candidates[0].inputs[0].edges[0].producer = 10; }), /inputs must list, per input group/);
  assert.match(broken((m) => { m.selected_candidates = { 9: [0] }; }), /9 is not a group of the last step/);
  assert.match(broken((m) => { m.selected_candidates = { 3: [10] }; }), /G3 must list candidate indices of the group/);
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
  // Pass 2 adds the tumbling-window chain G6 and its range G5, and a merge
  // in G4, one per θ of the three KLL sizes Pass 2 keeps.
  const pass2 = call('memoStepDiff', memo, PASS2);
  assert.deepEqual(pass2.groupsAdded, [5, 6]);
  assert.equal(pass2.candidatesDelta, 3 + 3 + 3);
  assert.equal(pass2.added, 9);
  assert.equal(call('memoStepText', memo, 0), '3 groups · 0 candidates');
  assert.equal(call('memoStepText', memo, PASS2), '7 groups (+2) · 21 candidates (+9) · 9 candidates new at this step');
});

test('stage 0 draws one box per group and an edge per child group', () => {
  const elements = call('memoStepElements', fixture(), 0, { queryIds: ['q1'] });
  assert.equal(nodesOf(elements).filter((e) => e.data.kind === 'group').length, 3);
  assert.deepEqual(edgesOf(elements).map((e) => e.data.id).sort(), ['g0>g1', 'g1>g2']);
  assert.match(byId(elements, 'g0').data.label, /^G0 · Aggregate quantile\(q=0\.9\)\nno candidates yet · root of q1$/);
  // Consumers sit above what they read.
  assert.ok(byId(elements, 'g0').position.y < byId(elements, 'g1').position.y);
  assert.equal(cytoscape(elements).nodes().length, 3);
});

test('each candidate sits inside its group, colored by origin, with θ, and an edge to each child group it reads', () => {
  const elements = call('memoStepElements', fixture(), PASS2, {});
  const kll = byId(elements, 'g4c6');
  assert.equal(kll.data.parent, 'g4');
  assert.equal(kll.data.color, vm.runInContext('MEMO_ORIGINS.pass1.color', context));
  assert.equal(kll.data.label, '#6 SummaryAgg Kll\nθ Kll(k=2863)\nPass1');
  // A candidate without θ says so on its middle line; a new one is marked
  // (its last line clipped).
  assert.equal(byId(elements, 'g0c1').data.label, '#1 Project\n—\nPass1 · ✓ selected');
  assert.equal(byId(elements, 'g4c7').data.label, '#7 SummaryMerge\nθ Kll(k=547)\nTumblingWindows · ✚ new · ✓ s…');
  assert.match(byId(elements, 'g4c7').classes, /added/);
  assert.equal(call('memoClip', 'abcdef', 4), 'abc…');
  assert.ok(byId(elements, 'g4c6>g1'));
  assert.ok(byId(elements, 'g4c7>g5'));
  // G5's three candidates all read G6: one edge, from the group.
  assert.deepEqual(edgesOf(elements).filter((e) => e.data.source.startsWith('g5')).map((e) => e.data.id), ['g5>g6']);
  const cy = cytoscape(elements);
  assert.equal(cy.getElementById('g4').children().length, 10);
  assert.equal(cy.edges().length, edgesOf(elements).length);
});

test("Pass 2's pruned candidates are marked and counted on their group", () => {
  const elements = call('memoStepElements', fixture(), PASS2, {});
  assert.deepEqual(nodesOf(elements).filter((e) => /\bpruned\b/.test(e.classes || '')).map((e) => e.data.id), ['g4c0', 'g4c1', 'g4c2', 'g4c3']);
  assert.match(byId(elements, 'g4c0').data.label, /Pass1 · ✗ pruned$/);
  assert.match(byId(elements, 'g4').data.label, /^G4 · SummaryAgg Kll\n10 candidates \(4 pruned by Pass 2\)$/);
  // Pass 1 does not prune.
  assert.ok(!call('memoStepElements', fixture(), 1, {}).some((e) => /pruned/.test(e.classes || '')));
});

test('selected plan only leaves the plan\'s candidates and their groups, on the last step only', () => {
  const elements = call('memoStepElements', fixture(), PASS2, { selectedOnly: true });
  // The plan reads the KLL chain; G1, the raw range only the exact
  // aggregate and the Pass 1 builds read, is left out with its edges.
  assert.deepEqual(nodesOf(elements).map((e) => e.data.id).sort(),
    ['g0', 'g0c1', 'g2', 'g2c0', 'g3', 'g3c0', 'g4', 'g4c7', 'g5', 'g5c0', 'g6', 'g6c0']);
  assert.ok(edgesOf(elements).every((e) => e.data.target !== 'g1'));
  // Earlier steps have no plan to filter by.
  assert.equal(call('memoStepElements', fixture(), 1, { selectedOnly: true }).length, call('memoStepElements', fixture(), 1, {}).length);
});

test('hiding pruned candidates leaves out them, groups with none left, and the edges into those groups', () => {
  const memo = fixture();
  // Say Pass 2 also pruned G1's only candidate.
  memo.steps[PASS2].groups[1].candidates[0].pruned = true;
  const elements = call('memoStepElements', memo, PASS2, { hidePruned: true });
  assert.deepEqual(nodesOf(elements).filter((e) => e.data.parent === 'g4').map((e) => e.data.id), ['g4c4', 'g4c5', 'g4c6', 'g4c7', 'g4c8', 'g4c9']);
  assert.match(byId(elements, 'g4').data.label, /\(4 pruned by Pass 2, hidden\)/);
  assert.ok(!byId(elements, 'g1') && !elements.some((e) => e.data.target === 'g1'));
});

test('a candidate\'s dimensions are its origin, sketch family, θ fields and window width', () => {
  const step = fixture().steps[PASS2];
  assert.deepEqual(call('memoCandidateDims', step.groups[4].candidates[5]), [['origin', 'Pass1'], ['sketch', 'Kll'], ['k', '1404']]);
  assert.deepEqual(call('memoCandidateDims', step.groups[4].candidates[7]), [['origin', 'TumblingWindows'], ['sketch', 'Kll'], ['k', '547']]);
  // G0's Project has no θ.
  assert.deepEqual(call('memoCandidateDims', step.groups[0].candidates[1]), [['origin', 'Pass1']]);
  // G6's chain has a window width.
  assert.deepEqual(call('memoCandidateDims', step.groups[6].candidates[0]).at(-1), ['window_ms', '60000']);
});

test('a large group splits by its first varying dimension; a count drills into it', () => {
  const memo = fixture();
  const inG4 = (elements) => nodesOf(elements).filter((e) => e.data.parent === 'g4');
  const split = call('memoStepElements', memo, PASS2, { collapseAbove: 4 });
  assert.deepEqual(inG4(split).map((e) => e.data.label), ['7 Pass1\n✗ 4 pruned', '3 TumblingWindows\n✚ 3 new · ✓ selected']);
  assert.match(byId(split, 'g4').data.label, /click a count to expand/);
  assert.match(byId(split, 'g4b1').classes, /chosen/);
  assert.ok(byId(split, 'g4b1>g5'));
  // Pass1's 7 candidates still exceed 4, so they split by the next varying dimension, k.
  const pass1 = [['origin', 'Pass1']];
  const deeper = call('memoStepElements', memo, PASS2, { collapseAbove: 4, drill: { 4: { path: pass1 } } });
  assert.deepEqual(inG4(deeper).map((e) => e.data.label.split('\n')[0]),
    ['1 · k=26', '1 · k=52', '1 · k=132', '1 · k=269', '1 · k=547', '1 · k=1404', '1 · k=2863']);
  assert.match(byId(deeper, 'g4').data.label, /click the group to go back\norigin=Pass1 \(7\)/);
  // With room for all 7, the drilled group shows Pass1's candidates themselves.
  const open = call('memoStepElements', memo, PASS2, { collapseAbove: 7, drill: { 4: { path: pass1 } } });
  assert.deepEqual(inG4(open).map((e) => e.data.id), ['g4c0', 'g4c1', 'g4c2', 'g4c3', 'g4c4', 'g4c5', 'g4c6']);
  assert.equal(cytoscape(open).getElementById('g4').children().length, 7);
  // Drilled in, a candidate shows the dimensions that still tell the shown ones apart.
  assert.equal(byId(open, 'g4c5').data.label.split('\n')[1], 'θ k=1404');
});

test('a group splits by the dimension chosen next; a candidate without it counts as "none", last', () => {
  const g4 = fixture().steps[PASS2].groups[4];
  // Say G4's last merge carried no θ.
  g4.candidates[9].parameters = null;
  const split = call('memoGroupDrill', g4.candidates, { path: [], next: 'k' }, 4);
  assert.equal(split.dim, 'k');
  assert.deepEqual(split.choices, ['origin', 'sketch', 'k']);
  // The merges share k with the Pass 1 builds they replace: 547 and 1404 twice.
  assert.deepEqual(split.buckets.map(([value, cs]) => [value, cs.length]),
    [['26', 1], ['52', 1], ['132', 1], ['269', 1], ['547', 2], ['1404', 2], ['2863', 1], ['none', 1]]);
  // A `next` that no longer varies falls back to the first that does.
  assert.equal(call('memoGroupDrill', g4.candidates, { path: [['origin', 'Pass1']], next: 'origin' }, 4).dim, 'k');
});

test('the selected candidates are highlighted on the last step only', () => {
  const memo = fixture();
  const last = call('memoStepElements', memo, PASS2, {});
  const chosen = nodesOf(last).filter((e) => /\bchosen\b/.test(e.classes)).map((e) => e.data.id).sort();
  assert.deepEqual(chosen, ['g0c1', 'g2c0', 'g3c0', 'g4c7', 'g5c0', 'g6c0']);
  assert.match(byId(last, 'g4c7>g5').classes, /chosen/);
  assert.ok(!call('memoStepElements', memo, 1, {}).some((e) => /chosen/.test(e.classes || '')));
  delete memo.selected_candidates;
  assert.ok(!call('memoStepElements', memo, PASS2, {}).some((e) => /chosen/.test(e.classes || '')));
});

test('a count marks only the selected candidate\'s edges', () => {
  const memo = fixture();
  // Another TumblingWindows candidate of G4 reads G1 instead of G5.
  memo.steps[PASS2].groups[4].candidates[8].children = [1];
  const elements = call('memoStepElements', memo, PASS2, { collapseAbove: 4 });
  assert.match(byId(elements, 'g4b1>g5').classes, /chosen/);
  assert.equal(byId(elements, 'g4b1>g1').classes, '');
});

test('highlighting a query dims the groups it does not reach', () => {
  const memo = fixture();
  memo.steps[PASS2].groups[5].queries = [];
  const elements = call('memoStepElements', memo, PASS2, {});
  const dimmed = new Set(call('memoQueryDimmedIds', elements, 0));
  assert.ok(dimmed.has('g5') && dimmed.has('g5c0') && dimmed.has('g4c7>g5'));
  assert.ok(!dimmed.has('g4') && !dimmed.has('g4c6') && !dimmed.has('g4c6>g1'));
});

test("a candidate's θ and required accuracy per input", () => {
  const groups = fixture().steps[PASS2].groups;
  assert.equal(call('memoParametersText', groups[4].candidates[5].parameters), 'θ Kll(k=1404)');
  assert.equal(call('memoParametersText', null), null);
  assert.deepEqual(call('memoRequiredAccuracyTexts', groups[3].candidates[0]), ['G4: rank ≤ 0.01, δ 0.00001']);
  assert.deepEqual(call('memoRequiredAccuracyTexts', groups[4].candidates[5]), ['G1: none']);
});

test("Pass 2's logical edges into a candidate: each producer of the input group, with α^log and why it is 0", () => {
  const estimate = fixture().steps[PASS2].groups[3].candidates[0];
  const [input] = call('memoLogicalEdges', estimate);
  assert.equal(input.title, 'input G4 · required rank ≤ 0.01, δ 0.00001');
  assert.equal(input.edges.length, 10);
  assert.match(input.edges[0], /^✗ #0: α\^log = 0: the producer's guarantee exceeds Rank ≤ 0\.01/);
  assert.equal(input.edges[5], '✓ #5');
});

test("a candidate's physical variants as a timing × lifetime grid: which exist, which Stage 2 pruned, which the plan uses", () => {
  const cell = (ids, pruned, selected) => ({ ids, pruned, selected });
  const none = cell([], false, false);
  // G4's selected merge: query time only; the plan keeps its result for one run.
  assert.deepEqual(call('memoPhysicalGrid', fixtureDoc(), 4, 7), [[none, none, none], [cell([5], false, false), cell([6], false, true), cell([], true, false)]]);
  // The Scan: at ingestion time, not kept, read by the chain; no raw data
  // is kept, so every query-time variant is pruned.
  const pruned = cell([], true, false);
  assert.deepEqual(call('memoPhysicalGrid', fixtureDoc(), 2, 0), [[cell([2], false, true), none, pruned], [pruned, pruned, pruned]]);
  assert.equal(call('memoPhysicalGrid', { memo: fixture() }, 0, 0), null);
  // Drawn under each kept candidate's label with `physical`, not under a pruned one.
  const elements = call('memoStepElements', fixture(), PASS2, { physical: fixtureDoc() });
  assert.match(byId(elements, 'g4c7').data.grid, /^data:image\/svg\+xml;utf8,/);
  assert.match(byId(elements, 'g4c7').classes, /withGrid/);
  assert.equal(byId(elements, 'g4c0').data.grid, undefined);
  assert.ok(byId(elements, 'g4c7').data.h > byId(call('memoStepElements', fixture(), PASS2, {}), 'g4c7').data.h);
});

test("a candidate's physical candidates list each input's physical edges with α^phys, the selected one marked", () => {
  const doc = fixtureDoc();
  const physical = call('memoPhysicalCandidates', doc, 3, 0);
  assert.deepEqual(physical.map((p) => p.title), ['C3 (query_time, None)', 'C4 (query_time, OneEvaluation(Memory))']);
  // Only edges whose logical edge holds and whose producer Stage 2 kept:
  // the physical candidates of G4's three merges (no raw data is kept, so
  // no query-time build is left).
  const kept = ['C5', 'C6', 'C7', 'C8', 'C9', 'C10'];
  assert.deepEqual(physical[0].inputs, [{ title: 'input G4', edges: kept.map((c) => `✓ ${c}${c === 'C6' ? ' · selected' : ''}`) }]);
  assert.deepEqual(physical[1].inputs, [{ title: 'input G4', edges: kept.map((c) => `✓ ${c}`) }]);
  // Every physical edge of this plan holds; one that fails says why.
  const rejected = doc.stage2_physical_asap_optimization.physical_candidates[3];
  Object.assign(rejected.inputs[0].edges[0], { satisfies_physical_requirement: false,
    rejection: 'α^phys = 0: consumer (QueryTime, None) cannot read producer (QueryTime, OneEvaluation(Memory))' });
  const lines = call('memoPhysicalCandidates', doc, rejected.group, rejected.index_in_group).flatMap((p) => p.inputs.flatMap((i) => i.edges));
  assert.ok(lines.some((line) => /^✗ C\d+: α\^phys = 0: consumer \(/.test(line)), lines.join('\n'));
  // Pruned by Pass 2: Stage 2 made no physical candidate of it.
  assert.deepEqual(call('memoPhysicalCandidates', doc, 4, 0), []);
});

test('the pruned candidates of a group: Pass 2 with its root causes, Stage 2 and Stage 3 with their reasons', () => {
  const doc = fixtureDoc();
  const pruned = call('memoPrunedLines', doc, 4);
  assert.equal(pruned.pass2.length, 4);
  assert.match(pruned.pass2[0], /^#0: the producer's guarantee exceeds Rank ≤ 0\.01 \(δ 0\.00001\)$/);
  assert.ok(pruned.stage2.includes('#4 (query_time, AcrossEvaluations(Memory)): query time results not retained'), pruned.stage2.join('\n'));
  assert.ok(pruned.stage2.includes('#4 (query_time, None): input has no buildable producer'), pruned.stage2.join('\n'));
  assert.deepEqual(pruned.stage3, []);
  // A cause traced through eliminated consumers names them; a Stage 3
  // reason names the physical candidate that dominates it.
  doc.stage1_logical_asap_optimization.pruned_candidates[0].causes[0].through_pruned_candidates = [{ group: 3, index_in_group: 0 }, { group: 0, index_in_group: 1 }];
  doc.stage3_plan_selection.pruned_candidates = [
    { group: 4, index_in_group: 8, properties: { timing: 'query_time', lifetime: 'None' }, reason: { dominated_by: 5 } },
    { group: 4, index_in_group: 9, properties: { timing: 'query_time', lifetime: 'None' }, reason: 'usage_not_costable' }];
  const traced = call('memoPrunedLines', doc, 4);
  assert.match(traced.pass2[0], /^#0: through G3\[0\] → G0\[1\]: the producer's guarantee exceeds/);
  assert.deepEqual(call('memoPass2Causes', doc, 4, 1).length, 1);
  assert.deepEqual(call('memoPass2Causes', doc, 4, 5), []);
  assert.deepEqual(traced.stage3, ['#8 (query_time, None): dominated by C5', '#9 (query_time, None): usage not costable']);
  assert.deepEqual(call('memoPrunedLines', { memo: doc.memo }, 4), { pass2: [], stage2: [], stage3: [] });
});
