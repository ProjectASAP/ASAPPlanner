// The Enumerated DAGs panel: `memo.enumeration` of an asap-stage-pipeline/v1
// document, the complete candidate DAGs the memo stands for, as `stage_pipeline`
// enumerated and composed them. The document holds each listed choice's DAG,
// so the page composes nothing: a clicked row's `dag` is drawn in the Stage 1
// lane by app.js. The checks and the rows are DOM-free (enumeration.test.mjs);
// renderEnumeration fills the panel. Reads validateLogicalDag and uniqueIds
// from stages.js.

// Memo steps in pipeline order, with the names the header uses.
const MEMO_STEP_NAMES = { stage0: 'Stage 0', pass1: 'Pass 1', pass2: 'Pass 2', concrete_states: 'concrete states' };

function memoStepName(step) {
  return MEMO_STEP_NAMES[step] || step;
}

const enumerationIsObject = (value) => !!value && typeof value === 'object' && !Array.isArray(value);
const enumerationIsCount = (value) => Number.isInteger(value) && value >= 0;

// A `MemoChoice`: group id (a string of digits) → candidate index.
function isMemoChoice(choice) {
  return enumerationIsObject(choice) && Object.entries(choice).every(([group, index]) => /^\d+$/.test(group) && enumerationIsCount(index));
}

// Every shape problem of `memo.enumeration`, as readable strings; empty when
// the document has none (a document without `memo` has none).
function validateMemoEnumeration(doc) {
  const memo = doc && doc.memo;
  if (memo === undefined) return [];
  if (!enumerationIsObject(memo)) return ['memo must be an object'];
  const enumeration = memo.enumeration;
  if (enumeration === undefined) return [];
  const at = 'memo.enumeration';
  if (!enumerationIsObject(enumeration)) return [`${at} must be an object`];
  const errors = [];
  const perStep = enumeration.per_step;
  if (!enumerationIsObject(perStep)) errors.push(`${at}.per_step must be an object`);
  else {
    Object.entries(perStep).forEach(([step, entry]) => {
      if (!entry || !enumerationIsCount(entry.count) || typeof entry.complete !== 'boolean') errors.push(`${at}.per_step.${step} needs a count and complete`);
    });
  }
  if (typeof enumeration.step !== 'string' || !enumerationIsObject(perStep) || !(enumeration.step in perStep)) {
    errors.push(`${at}.step must name a step of per_step`);
  }
  if (!Number.isInteger(enumeration.limit) || enumeration.limit < 1) errors.push(`${at}.limit must be a positive integer`);
  const priced = enumeration.priced;
  if (priced !== undefined && priced !== null && !(enumerationIsObject(priced) && enumerationIsCount(priced.count) && enumerationIsCount(priced.cheaper_than_selected)
      && (priced.selected_cost_per_second === null || Number.isFinite(priced.selected_cost_per_second)))) {
    errors.push(`${at}.priced must be null or {count, cheaper_than_selected, selected_cost_per_second}`);
  }
  if (!Array.isArray(enumeration.choices)) return errors.concat(`${at}.choices must be an array`);
  uniqueIds(enumeration.choices, `${at}.choices`, errors);
  const counted = enumerationIsObject(perStep) && perStep[enumeration.step];
  if (counted && enumerationIsCount(counted.count) && enumeration.choices.length > counted.count) {
    errors.push(`${at}: ${enumeration.choices.length} choices listed but per_step.${enumeration.step} counts ${counted.count}`);
  }
  enumeration.choices.forEach((entry, index) => {
    if (!enumerationIsObject(entry)) return;
    const where = `${at}.choices[${index}]`;
    if (!isMemoChoice(entry.choice)) errors.push(`${where}.choice must map group ids to candidate indices`);
    // One root per root group, which need not be one per query.
    const roots = entry.dag && Array.isArray(entry.dag.roots) ? entry.dag.roots.length : 0;
    errors.push(...validateLogicalDag(entry.dag, where, roots));
    ['budget_share_1', 'selected'].forEach((field) => {
      if (typeof entry[field] !== 'boolean') errors.push(`${where}.${field} must be true or false`);
    });
    // Present only when the run priced the enumerated DAGs.
    ['valid', 'cheaper_than_selected'].forEach((field) => {
      if (entry[field] !== undefined && typeof entry[field] !== 'boolean') errors.push(`${where}.${field} must be true or false`);
    });
    if (entry.rejection !== undefined && entry.rejection !== null && typeof entry.rejection !== 'string') errors.push(`${where}.rejection must be a string or null`);
    if (entry.cost_per_second !== undefined && entry.cost_per_second !== null && !Number.isFinite(entry.cost_per_second)) {
      errors.push(`${where}.cost_per_second must be a number or null`);
    }
  });
  return errors;
}

// "343", or "≥ 1000" when the count stopped at the limit.
function enumerationCountText(entry, limit) {
  return entry.complete ? String(entry.count) : `≥ ${limit}`;
}

// The panel's content, or null for a document without `memo.enumeration`:
// `counts` ("Stage 0: 1 · Pass 1: 343 · Pass 2: ≥ 1000"), `listing` (what the
// rows are), `priced` (the pricing summary, "" when not priced), and one row
// per listed choice. A row's `changes` are the groups whose candidate differs
// from `selected_choice` (every group of the choice when there is none), each
// with the candidate's origin and label when the enumerated step's memo
// lists that candidate. Both choices are sparse: a group a choice does not
// name takes candidate 0. `selected_choice` is over the last memo step; when
// the enumerated step is an earlier one, only the row's own groups are
// compared and `against` says the comparison crosses steps.
function enumerationView(doc) {
  const memo = doc && doc.memo;
  const enumeration = memo && memo.enumeration;
  if (!enumeration) return null;
  const known = Object.keys(MEMO_STEP_NAMES);
  const steps = Object.keys(enumeration.per_step)
    .sort((a, b) => (known.indexOf(a) + 1 || known.length + 1) - (known.indexOf(b) + 1 || known.length + 1));
  const counts = steps.map((step) => `${memoStepName(step)}: ${enumerationCountText(enumeration.per_step[step], enumeration.limit)}`).join(' · ');
  const total = enumeration.per_step[enumeration.step];
  const name = memoStepName(enumeration.step);
  const shown = enumeration.choices.length;
  const listing = total.complete && shown === total.count
    ? `listed: all ${shown} ${name} DAGs`
    : `listed: the first ${shown} of ${enumerationCountText(total, enumeration.limit)} ${name} DAGs`;
  const priced = enumeration.priced
    ? `${enumeration.priced.count} priced · ${enumeration.priced.cheaper_than_selected} cheaper than the selected plan` +
      (enumeration.priced.selected_cost_per_second === null ? '' : ` (${Number(enumeration.priced.selected_cost_per_second.toPrecision(4))})`)
    : '';

  const selected = enumerationIsObject(memo.selected_choice) ? memo.selected_choice : null;
  const step = (Array.isArray(memo.steps) ? memo.steps : []).find((s) => s && s.step === enumeration.step);
  const candidateOf = (group, index) => {
    const g = step && Array.isArray(step.groups) ? step.groups.find((x) => x && String(x.id) === group) : null;
    return g && Array.isArray(g.candidates) ? g.candidates.find((c) => c && c.index === index) : null;
  };
  const memoSteps = Array.isArray(memo.steps) ? memo.steps.filter((s) => s && typeof s.step === 'string') : [];
  const lastStep = memoSteps.length ? memoSteps[memoSteps.length - 1].step : steps[steps.length - 1];
  const sameStep = enumeration.step === lastStep;
  const rows = enumeration.choices.map((entry) => {
    const groups = new Set(Object.keys(entry.choice));
    if (selected && sameStep) Object.keys(selected).forEach((group) => groups.add(group));
    const changes = [...groups]
      .filter((group) => !selected || (entry.choice[group] ?? 0) !== (selected[group] ?? 0))
      .sort((a, b) => Number(a) - Number(b))
      .map((group) => {
        // A group of the selected choice this DAG does not read.
        if (!(group in entry.choice)) return { group: Number(group), index: null, origin: null, label: null };
        const index = entry.choice[group];
        const candidate = candidateOf(group, index);
        return { group: Number(group), index, origin: candidate ? candidate.origin : null, label: candidate ? candidate.label : null };
      });
    return {
      id: entry.id,
      changes,
      text: changes.length
        ? changes.map((c) => (c.index === null ? `G${c.group} not read`
          : `G${c.group} → #${c.index}${c.label ? ` ${c.label}` : ''}${c.origin ? ` (${c.origin})` : ''}`)).join(' · ')
        : 'the selected choice',
      nodes: entry.dag.nodes.length,
      budgetShare1: entry.budget_share_1,
      selected: entry.selected,
      priced: 'valid' in entry,
      valid: entry.valid,
      rejection: entry.rejection || null,
      cost: typeof entry.cost_per_second === 'number' ? entry.cost_per_second : null,
      cheaper: entry.cheaper_than_selected === true,
    };
  });
  const against = !selected
    ? `every group's candidate (no selected choice${memo.selected_choice_unavailable ? `: ${memo.selected_choice_unavailable}` : ''})`
    : sameStep ? 'groups whose candidate differs from the selected plan’s choice'
      : `groups whose candidate differs from the selected plan’s choice, which is over the ${memoStepName(lastStep)} memo: its group ids and candidate indices may not match ${name}'s, and “selected” is not meaningful`;
  return { counts, listing, priced, against, rows };
}

// The panel's state: the document's view, the row drawn in the Stage 1
// lane, and app.js's function that draws an enumerated DAG there.
const enumerationPanel = { doc: null, view: null, pressed: null, draw: null, bound: false };

function enumerationRowHtml(row, pressed) {
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const chips = [
    row.selected ? '<span class="chip selected">✓ selected</span>' : '',
    row.cheaper ? '<span class="chip cheaper">cheaper than selected</span>' : '',
    row.priced && row.valid === false ? '<span class="chip invalid">✗ invalid</span>' : '',
    row.priced && row.valid === true && !row.selected && !row.cheaper ? '<span class="chip costlier">valid</span>' : '',
    row.budgetShare1 ? '' : '<span class="chip">budget share &lt; 1</span>',
  ].join('');
  const cost = row.cost === null ? (row.priced ? '—' : '') : String(Number(row.cost.toPrecision(4)));
  return `<li><button type="button" data-id="${esc(row.id)}" aria-pressed="${row.id === pressed}"${row.cheaper ? ' class="cheaper"' : ''}>` +
    `<span class="pid">${esc(row.id)}</span><span class="plabel">${esc(row.text)}</span><span class="ptotal">${cost}</span>` +
    `<span class="pstatus">${chips}${row.rejection ? `<span>${esc(row.rejection.replace(/_/g, ' '))}</span>` : ''}<span>${row.nodes} nodes</span></span></button></li>`;
}

// Mark row `id` (null: none) as the one the Stage 1 lane draws.
function pressEnumeratedRow(id) {
  const state = enumerationPanel;
  state.pressed = id;
  if (state.view) document.getElementById('enumRows').innerHTML = state.view.rows.map((row) => enumerationRowHtml(row, id)).join('');
}

// Fill the panel for `doc` (hidden without `memo.enumeration`); a click on a
// row calls `draw(id, dag, description)` and marks the row.
function renderEnumeration(doc, draw) {
  const $ = (id) => document.getElementById(id);
  const state = enumerationPanel;
  state.doc = doc;
  state.view = enumerationView(doc);
  state.draw = draw;
  $('enumPanel').hidden = !state.view;
  if (!state.view) { state.pressed = null; $('enumRows').innerHTML = ''; return; }
  $('enum-meta').textContent = [state.view.counts, state.view.listing, state.view.priced, `each row: ${state.view.against}`].filter(Boolean).join(' · ');
  pressEnumeratedRow(null);
  if (state.bound) return;
  state.bound = true;
  // One listener for every document: rows are rewritten on each load.
  $('enumRows').addEventListener('click', (event) => {
    const button = event.target && event.target.closest ? event.target.closest('button[data-id]') : null;
    if (!button || !state.view) return;
    const choice = state.doc.memo.enumeration.choices.find((c) => c.id === button.dataset.id);
    const row = state.view.rows.find((r) => r.id === button.dataset.id);
    state.draw(choice.id, choice.dag, row.text);
    pressEnumeratedRow(choice.id);
  });
}
