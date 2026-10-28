// Node tests of enumeration.js (no DOM): the checks of `memo.enumeration` and
// the Enumerated DAGs panel's rows, over a hand-written priced memo on #509
// Example 1 (test-fixtures/memo-enumeration-small.json). Run from the
// repository root: `node --test tools/dag-viewer/enumeration.test.mjs`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const here = new URL('.', import.meta.url);
const context = vm.createContext({ console: { warn() {} } });
for (const file of ['node-style.js', 'stages.js', 'enumeration.js']) {
  vm.runInContext(readFileSync(new URL(file, here), 'utf8'), context, { filename: file });
}
const call = (name, ...args) => JSON.parse(JSON.stringify(context[name](...args)));
const read = (path) => JSON.parse(readFileSync(new URL(path, here), 'utf8'));
const example1 = read('examples/planner-layering-example1.json');
const priced = () => ({ ...structuredClone(example1), ...read('test-fixtures/memo-enumeration-small.json') });
// The same memo as a run without --price-enumerated writes it.
const unpriced = () => {
  const doc = priced();
  doc.memo.enumeration.priced = null;
  doc.memo.enumeration.choices.forEach((c) => { delete c.valid; delete c.rejection; delete c.cost_per_second; delete c.cheaper_than_selected; });
  return doc;
};

test('documents without memo, or memo without enumeration, have no enumeration problems', () => {
  assert.deepEqual(call('validateMemoEnumeration', example1), []);
  assert.deepEqual(call('validateMemoEnumeration', { ...example1, memo: { steps: [] } }), []);
  assert.equal(call('enumerationView', example1), null);
});

test('the priced and unpriced fixtures are valid', () => {
  assert.deepEqual(call('validateMemoEnumeration', priced()), []);
  assert.deepEqual(call('validateMemoEnumeration', unpriced()), []);
});

test('each broken field of memo.enumeration is reported', () => {
  const cases = {
    'memo must be an object': (d) => { d.memo = []; },
    'per_step.pass1 needs a count and complete': (d) => { d.memo.enumeration.per_step.pass1.complete = 'yes'; },
    'step must name a step of per_step': (d) => { d.memo.enumeration.step = 'pass9'; },
    'limit must be a positive integer': (d) => { d.memo.enumeration.limit = 0; },
    'choices must be an array': (d) => { d.memo.enumeration.choices = {}; },
    'duplicate id E1': (d) => { d.memo.enumeration.choices[1].id = 'E1'; },
    '5 choices listed but per_step.pass1 counts 4': (d) => { d.memo.enumeration.choices.push({ ...d.memo.enumeration.choices[0], id: 'E5' }); },
    'choices[0].choice must map group ids to candidate indices': (d) => { d.memo.enumeration.choices[0].choice = { G7: 0 }; },
    'choices[0].roots must index memo.nodes': (d) => { d.memo.enumeration.choices[0].roots = [d.memo.nodes.length]; },
    'choices[1].roots must index memo.nodes': (d) => { delete d.memo.enumeration.choices[1].roots; },
    'choices[0].budget_share_1 must be true or false': (d) => { delete d.memo.enumeration.choices[0].budget_share_1; },
    'choices[0].cheaper_than_selected must be true or false': (d) => { d.memo.enumeration.choices[0].cheaper_than_selected = 1; },
    'choices[0].cost_per_second must be a number or null': (d) => { d.memo.enumeration.choices[0].cost_per_second = 'cheap'; },
    'choices[0].rejection must be a string or null': (d) => { d.memo.enumeration.choices[0].rejection = 3; },
    'priced must be null or': (d) => { d.memo.enumeration.priced = { count: 4 }; },
  };
  for (const [message, breakIt] of Object.entries(cases)) {
    const doc = priced();
    breakIt(doc);
    const errors = call('validateMemoEnumeration', doc);
    assert.ok(errors.some((e) => e.includes(message)), `${message}: got ${JSON.stringify(errors)}`);
  }
});

test('the header counts every step, with ≥ and the limit when the count stopped', () => {
  const view = call('enumerationView', unpriced());
  assert.equal(view.counts, 'Stage 0: 1 · Pass 1: 4');
  assert.equal(view.listing, 'listed: all 4 Pass 1 DAGs');
  assert.equal(view.priced, '');
  const capped = unpriced();
  capped.memo.enumeration.per_step.pass1 = { count: 1000, complete: false };
  capped.memo.enumeration.per_step.pass2 = { count: 1000, complete: false };
  const cappedView = call('enumerationView', capped);
  assert.equal(cappedView.counts, 'Stage 0: 1 · Pass 1: ≥ 1000 · Pass 2: ≥ 1000');
  assert.equal(cappedView.listing, 'listed: the first 4 of ≥ 1000 Pass 1 DAGs');
});

test('choices are sparse: a group either choice leaves out takes candidate 0', () => {
  const doc = unpriced();
  doc.memo.selected_choice = { 7: 1 };
  const choices = doc.memo.enumeration.choices;
  choices[0].choice = { 3: 0, 7: 0 };
  delete choices[1].choice[2];
  choices[2].choice = { 7: 2, 2: 0 };
  const rows = call('enumerationView', doc).rows;
  assert.equal(rows[0].text, 'G7 → #0 exact accumulator (pass1)');
  assert.equal(rows[1].text, 'the selected choice');
  assert.equal(rows[2].text, 'G7 → #2 CountSketch+heap (pass1)');
});

test('a group of the selected choice that a DAG does not read is named', () => {
  const doc = unpriced();
  doc.memo.selected_choice = { 7: 1, 9: 2 };
  assert.equal(call('enumerationView', doc).rows[1].text, 'G9 not read');
});

test('a selected choice over a later step than the enumerated one is flagged', () => {
  const doc = unpriced();
  doc.memo.steps.push({ step: 'pass2', title: 'Pass 2', groups: [] });
  doc.memo.enumeration.per_step.pass2 = { count: 1000, complete: false };
  doc.memo.selected_choice = { 7: 1, 9: 2 };
  const view = call('enumerationView', doc);
  assert.match(view.against, /over the Pass 2 memo/);
  assert.ok(view.rows.every((r) => !r.selected));
  // Only the row's own groups are compared across steps.
  assert.equal(view.rows[1].text, 'the selected choice');
});

test('a row names the groups whose candidate differs from the selected choice, with origin and label', () => {
  const rows = call('enumerationView', unpriced()).rows;
  assert.deepEqual(rows.map((r) => r.id), ['E1', 'E2', 'E3', 'E4']);
  assert.equal(rows[0].text, 'G7 → #0 exact accumulator (pass1)');
  assert.equal(rows[1].text, 'the selected choice');
  assert.equal(rows[1].selected, true);
  assert.equal(rows[3].text, 'G7 → #3 CMS+heap shared build (shared_build)');
  assert.equal(rows[3].budgetShare1, false);
  assert.ok(rows.every((r) => !r.priced && r.cost === null && !r.cheaper));
});

test('a choice that does not compose has its error instead of a DAG and is not clickable', () => {
  const doc = unpriced();
  doc.memo.enumeration.choices[3].roots = null;
  assert.ok(call('validateMemoEnumeration', doc).some((e) => e.includes('choices[3]: needs roots or an error')));
  doc.memo.enumeration.choices[3].error = 'cycle';
  assert.deepEqual(call('validateMemoEnumeration', doc), []);
  const row = call('enumerationView', doc).rows[3];
  assert.equal(row.error, 'cycle');
  assert.match(context.enumerationRowHtml(row, null), /disabled>.*does not compose: cycle/);
});

test('without the step in memo.steps a row names only the candidate index', () => {
  const doc = unpriced();
  doc.memo.steps = [];
  assert.equal(call('enumerationView', doc).rows[2].text, 'G7 → #2');
});

test('without a selected choice a row names every group of its choice', () => {
  const doc = unpriced();
  doc.memo.selected_choice = null;
  doc.memo.selected_choice_unavailable = 'no plan selected';
  const view = call('enumerationView', doc);
  assert.equal(view.rows[0].changes.length, 8);
  assert.match(view.against, /no plan selected/);
});

test('a priced row carries its cost, validity and whether it is cheaper than the selected plan', () => {
  const view = call('enumerationView', priced());
  assert.equal(view.priced, '4 priced · 1 cheaper than the selected plan (2)');
  const byId = Object.fromEntries(view.rows.map((r) => [r.id, r]));
  assert.deepEqual([byId.E3.cost, byId.E3.valid, byId.E3.cheaper], [1.5, true, true]);
  assert.deepEqual([byId.E4.cost, byId.E4.valid, byId.E4.rejection], [null, false, 'accuracy_target_missed']);
  const html = context.enumerationRowHtml(view.rows[2], null);
  assert.match(html, /class="cheaper"/);
  assert.match(html, /cheaper than selected/);
  assert.doesNotMatch(context.enumerationRowHtml(view.rows[0], null), /cheaper/);
});
