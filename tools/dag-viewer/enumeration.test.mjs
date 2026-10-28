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
    'choices[0]: node 1 reads missing node 42': (d) => { Object.values(d.memo.enumeration.choices[0].dag.nodes[1].operator)[0].TimeRange.child = 42; },
    'choices[0]: dag needs nodes and roots': (d) => { delete d.memo.enumeration.choices[0].dag; },
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
  assert.equal(view.counts, 'Stage 0: 1 · Pass 1: 4 · Pass 2: ≥ 1000');
  assert.equal(view.listing, 'Pass 1: all 4 DAGs listed');
  assert.equal(view.priced, '');
  const capped = unpriced();
  capped.memo.enumeration.per_step.pass1 = { count: 1000, complete: false };
  assert.equal(call('enumerationView', capped).listing, 'Pass 1: first 4 of ≥ 1000 DAGs listed');
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
