// The Memo view of the Stage Viewer: the document's optional `memo` section
// (written by `asap_planner`) drawn after each memo step, one tab per
// step. Each memo group is a box holding its candidates; an edge goes from
// a candidate to each input group it reads. The functions up to
// `memoViewController` are DOM-free (memo.test.mjs and test_viewer.py run
// them headless); the controller draws them on the page.
//
// The section, per step: `{step, title, groups: [{id, label, operator,
// children, parents, queries, roots_of, candidate_count,
// candidate_count_by_origin, candidates: [{index, origin, added, label,
// operator, children, fused_groups, parameters, required_accuracy}]}]}`. A
// group's `operator` is its expression and a candidate's `operator` one
// operator, both with group ids as children; a candidate's `children` are
// its input groups, with one `required_accuracy` each (null or `{metric,
// max_error, failure_probability}`), and `parameters` is its θ (null when
// exact). At the `pass2` step a candidate also has `pruned` and, per input,
// `inputs[i].edges`: the logical edge from every candidate of the input
// group (`producer`, its index there), with `satisfies_logical_requirement`
// (α^log) and `rejection` (why α^log = 0). `selected_candidates` maps a
// group id (string) of the last step to the indices of the plan's
// candidates there (one per execution context using the group); it is
// absent when no plan was selected.
//
// Outside the section, the memo view reads Pass 2's `pruned_candidates`
// (`group`, `index_in_group` and `causes`, each a root cause `text` reached
// `through_pruned_consumers`), Stage 2's `physical_candidates` (a memo
// candidate's `group` and `index_in_group` with physical `properties` and,
// per input, each physical edge whose logical edge holds, with α^phys and
// its `rejection`) and the candidates Stage 2 and Stage 3 pruned (`group`,
// `index_in_group`, `properties`, `reason`).

// Memo steps in pipeline order; a step production does not run is omitted.
const MEMO_STEPS = ['stage0', 'pass1', 'pass2'];

// `OriginOfCandidateForMemoGroup` (snake_case in the document), its Rust name, and the
// border color of its candidates (mid tones, readable in both themes).
const MEMO_ORIGINS = {
  pass1: { name: 'Pass1', color: '#4f7bd9' },
  tumbling_windows: { name: 'TumblingWindows', color: '#d4524c' },
  rollup: { name: 'Rollup', color: '#1d9fb0' },
  top_k_reuse: { name: 'TopKReuse', color: '#b8931c' },
};

// A group with more candidates than this is drawn collapsed: one node per
// origin with its count.
const MEMO_COLLAPSE_ABOVE = 12;

const memoIsIndexList = (ids) => Array.isArray(ids) && ids.every((id) => Number.isInteger(id) && id >= 0);
const memoIsObject = (value) => !!value && typeof value === 'object' && !Array.isArray(value);

// `SketchParams` JSON, e.g. `{"Kll": {"k": 269}}`, as "Kll(k=269)".
function memoSketchParamsText(parameters) {
  return Object.entries(parameters).map(([family, fields]) => (fields && typeof fields === 'object'
    ? `${family}(${Object.entries(fields).map(([k, v]) => `${k}=${compactWire(v)}`).join(', ')})` : compactWire({ [family]: fields }))).join(', ');
}

// A candidate's parameters θ as "θ Kll(k=269)"; null without any.
function memoParametersText(parameters) {
  return parameters ? `θ ${memoSketchParamsText(parameters)}` : null;
}

// An input's required accuracy (`{metric, max_error, failure_probability}`)
// as "rank ≤ 0.01, δ 0.00001"; "none" for null.
function memoRequiredText(required) {
  return required ? `${required.metric} ≤ ${required.max_error}, δ ${required.failure_probability}` : 'none';
}

// A candidate's required accuracy per input, one "G<input>: …" each.
function memoRequiredAccuracyTexts(candidate) {
  return candidate.children.map((group, i) => `G${group}: ${memoRequiredText(candidate.required_accuracy[i])}`);
}

// Physical properties `{timing, lifetime}` as "query_time, OneEvaluation(Memory)".
function memoPropertiesText(properties) {
  return `${properties.timing}, ${compactWire(properties.lifetime)}`;
}

// Why Pass 2 eliminated candidate `index` of memo group `group`: one line
// per cause, its root cause's text and, when it was eliminated because its
// consumers were, the eliminated consumers it was traced through, e.g.
// "through G4[0] → G6[1]: the producer's guarantee exceeds …". Empty when
// Pass 2 kept it.
function memoPass2Causes(doc, group, index) {
  const pruned = ((doc.stage1_logical_asap_optimization || {}).pruned_candidates || [])
    .find((p) => p.group === group && p.index_in_group === index);
  return pruned ? pruned.causes.map((cause) => (cause.through_pruned_consumers.length
    ? `through ${cause.through_pruned_consumers.map((c) => `G${c.group}[${c.index_in_group}]`).join(' → ')}: ${cause.text}`
    : cause.text)) : [];
}

// The pruned candidates of memo group `group`, one line each, by the
// stage that pruned them: Pass 2's as "#<index>: cause" (one line per
// cause), Stage 2's and Stage 3's as "#<index> (timing, lifetime): reason".
function memoPrunedLines(doc, group) {
  const physical = (section) => ((section || {}).pruned_candidates || []).filter((p) => p.group === group)
    .map((p) => `#${p.index_in_group} (${memoPropertiesText(p.properties)}): ${prunedReasonText(p.reason)}`);
  return {
    pass2: ((doc.stage1_logical_asap_optimization || {}).pruned_candidates || []).filter((p) => p.group === group)
      .flatMap((p) => memoPass2Causes(doc, group, p.index_in_group).map((cause) => `#${p.index_in_group}: ${cause}`)),
    stage2: physical(doc.stage2_physical_asap_optimization),
    stage3: physical(doc.stage3_plan_selection),
  };
}

// The logical edges into candidate `candidate` of the Pass 2 step, per
// input: "✓ #<producer>" when α^log = 1, "✗ #<producer>: rejection"
// otherwise, the producer an index in the input group.
function memoLogicalEdges(candidate) {
  return candidate.inputs.map((input, i) => ({
    title: `input G${input.group} · required ${memoRequiredText(candidate.required_accuracy[i])}`,
    edges: input.edges.map((e) => `${e.satisfies_logical_requirement ? '✓' : '✗'} #${e.producer}${e.rejection === null ? '' : `: ${e.rejection}`}`),
  }));
}

// The physical candidates Stage 2 made of candidate `index` of memo group
// `group`: each `C<id> (timing, lifetime)` and, per input, each physical
// edge whose logical edge holds: "✓ C<producer>" when α^phys = 1 or
// "✗ C<producer>: rejection", marked "selected" when the plan uses it.
function memoPhysicalCandidates(doc, group, index) {
  return ((doc.stage2_physical_asap_optimization || {}).physical_candidates || [])
    .filter((c) => c.group === group && c.index_in_group === index)
    .map((c) => ({
      title: `C${c.id} (${memoPropertiesText(c.properties)})`,
      inputs: c.inputs.map((input) => ({
        title: `input G${input.group}`,
        edges: input.edges.map((e) => `${e.satisfies_physical_requirement ? '✓' : '✗'} C${e.producer}${e.rejection === null ? '' : `: ${e.rejection}`}${e.selected ? ' · selected' : ''}`),
      })),
    }));
}

// The checks of `memo.steps` and `memo.selected_candidates` against the
// format above, as messages ([] when it holds or the document has no
// memo). `queryCount` is the workload's query count.
function validateMemo(memo, queryCount) {
  if (memo === undefined) return [];
  if (!memoIsObject(memo)) return ['memo must be an object'];
  if (!Array.isArray(memo.steps) || memo.steps.length === 0) return ['memo.steps must be a non-empty array'];
  const errors = [];
  let previous = -1;
  memo.steps.forEach((step, s) => {
    const at = `memo.steps[${s}]`;
    if (!step || typeof step !== 'object') { errors.push(`${at} must be an object`); return; }
    const position = MEMO_STEPS.indexOf(step.step);
    if (typeof step.title !== 'string') errors.push(`${at}: title must be a string`);
    if (position < 0) errors.push(`${at}: step must be one of ${MEMO_STEPS.join(', ')}`);
    else if (position <= previous) errors.push(`${at}: step ${step.step} is out of pipeline order or repeated`);
    else previous = position;
    if (!Array.isArray(step.groups)) { errors.push(`${at}: groups must be an array`); return; }
    const ids = new Set();
    const counts = new Map();
    step.groups.forEach((group, g) => {
      if (!group || !Number.isInteger(group.id) || group.id < 0) errors.push(`${at}.groups[${g}]: id must be a group id`);
      else if (ids.has(group.id)) errors.push(`${at}.groups[${g}]: duplicate group id ${group.id}`);
      else { ids.add(group.id); counts.set(group.id, group.candidate_count); }
    });
    step.groups.forEach((group) => {
      if (!group || !ids.has(group.id)) return;
      const where = `${at} G${group.id}`;
      if (typeof group.label !== 'string' || !memoIsObject(group.operator)) errors.push(`${where}: needs a label and an operator`);
      const names = (list) => memoIsIndexList(list) && list.every((id) => ids.has(id));
      if (!names(group.children) || !names(group.parents)) errors.push(`${where}: children and parents must name groups of the step`);
      if (!memoIsIndexList(group.queries) || !memoIsIndexList(group.roots_of)
        || ![...group.queries, ...group.roots_of].every((q) => q < queryCount)) errors.push(`${where}: queries and roots_of must name workload queries`);
      if (!Array.isArray(group.candidates)) { errors.push(`${where}: candidates must be an array`); return; }
      const count = group.candidate_count;
      if (count !== group.candidates.length) errors.push(`${where}: candidate_count must count the candidates`);
      const byOrigin = group.candidate_count_by_origin;
      if (!memoIsObject(byOrigin) || Object.keys(byOrigin).some((o) => !(o in MEMO_ORIGINS))
        || Object.values(byOrigin).reduce((n, c) => n + c, 0) !== count) errors.push(`${where}: candidate_count_by_origin must count candidate_count by known origin`);
      const indices = new Set();
      group.candidates.forEach((candidate, c) => {
        const cw = `${where} candidates[${c}]`;
        if (!candidate || !Number.isInteger(candidate.index) || candidate.index < 0 || indices.has(candidate.index)) {
          errors.push(`${cw}: index must be a unique candidate index`);
          return;
        }
        indices.add(candidate.index);
        if (!(candidate.origin in MEMO_ORIGINS)) errors.push(`${cw}: unknown origin ${JSON.stringify(candidate.origin)}`);
        else if (!(byOrigin && byOrigin[candidate.origin] > 0)) errors.push(`${cw}: origin ${candidate.origin} is not counted in candidate_count_by_origin`);
        if (typeof candidate.label !== 'string' || !memoIsObject(candidate.operator)) errors.push(`${cw}: needs a label and an operator`);
        if (!names(candidate.children) || !names(candidate.fused_groups)) errors.push(`${cw}: children and fused_groups must name groups of the step`);
        if (typeof candidate.added !== 'boolean') errors.push(`${cw}: added must be a boolean`);
        if (!(candidate.parameters === null || memoIsObject(candidate.parameters))) errors.push(`${cw}: parameters must be an object or null`);
        const required = candidate.required_accuracy;
        if (!Array.isArray(required) || !Array.isArray(candidate.children) || required.length !== candidate.children.length
          || !required.every((r) => r === null || (memoIsObject(r) && typeof r.metric === 'string' && typeof r.max_error === 'number'))) {
          errors.push(`${cw}: required_accuracy must hold one null or {metric, max_error, failure_probability} per input`);
        }
        // Pass 2 lists, per input, the logical edge from every candidate of the input group.
        if (step.step !== 'pass2') return;
        if (typeof candidate.pruned !== 'boolean') errors.push(`${cw}: pruned must be a boolean`);
        const inputs = candidate.inputs;
        if (!Array.isArray(inputs) || !Array.isArray(candidate.children) || inputs.length !== candidate.children.length
          || !inputs.every((input, i) => memoIsObject(input) && input.group === candidate.children[i] && Array.isArray(input.edges)
            && input.edges.length === counts.get(input.group) && input.edges.every((e) => memoIsObject(e)
              && Number.isInteger(e.producer) && e.producer >= 0 && e.producer < counts.get(input.group)
              && typeof e.satisfies_logical_requirement === 'boolean' && (e.rejection === null || typeof e.rejection === 'string')))) {
          errors.push(`${cw}: inputs must list, per input group, the logical edge from each of its candidates`);
        }
      });
    });
  });
  if (errors.length || memo.selected_candidates === undefined) return errors;
  const selected = memo.selected_candidates;
  if (!memoIsObject(selected)) return ['memo.selected_candidates must be an object'];
  const last = new Map(memo.steps[memo.steps.length - 1].groups.map((group) => [String(group.id), group]));
  Object.entries(selected).forEach(([group, indices]) => {
    const g = last.get(group);
    if (!g) errors.push(`memo.selected_candidates: ${group} is not a group of the last step`);
    else if (!Array.isArray(indices) || !indices.length || !indices.every((i) => Number.isInteger(i) && i >= 0 && i < g.candidate_count)) {
      errors.push(`memo.selected_candidates: G${group} must list candidate indices of the group`);
    }
  });
  return errors;
}

// What step `s` changed: its groups, the groups it added and removed over
// step `s - 1` (all of them added at the first step), its candidates and
// their net change (`candidatesDelta`), and the candidates it added
// (`added`).
function memoStepDiff(memo, s) {
  const groups = memo.steps[s].groups;
  const before = s > 0 ? new Map(memo.steps[s - 1].groups.map((g) => [g.id, g])) : new Map();
  const candidates = groups.reduce((n, g) => n + g.candidate_count, 0);
  const candidatesDelta = groups.reduce((n, g) => n + g.candidate_count - (before.has(g.id) ? before.get(g.id).candidate_count : 0), 0);
  return {
    groups: groups.length,
    groupsAdded: groups.filter((g) => !before.has(g.id)).map((g) => g.id),
    groupsRemoved: [...before.keys()].filter((id) => !groups.some((g) => g.id === id)),
    candidates,
    candidatesDelta,
    added: groups.reduce((n, g) => n + g.candidates.filter((c) => c.added).length, 0),
  };
}

// The step's diff as one line.
function memoStepText(memo, s) {
  const d = memoStepDiff(memo, s);
  const plus = (n) => (s > 0 ? ` (${n >= 0 ? '+' : ''}${n})` : '');
  return `${d.groups} groups${plus(d.groupsAdded.length - d.groupsRemoved.length)} · ${d.candidates} candidates${plus(d.candidatesDelta)}`
    + (d.added ? ` · ${d.added} candidates new at this step` : '');
}

// `text` cut to `n` characters, with an ellipsis when cut.
function memoClip(text, n) {
  const s = String(text);
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}

// Each group's rank in the drawing: 0 without a child group in the step,
// else one above its highest child (through its own children and its
// candidates'). A cycle, which a memo should not have, is cut.
function memoGroupRanks(groups) {
  const byId = new Map(groups.map((g) => [g.id, g]));
  const reads = (g) => [...new Set([...g.children, ...g.candidates.flatMap((c) => c.children)])].filter((id) => byId.has(id) && id !== g.id);
  const rank = new Map();
  const visiting = new Set();
  const visit = (g) => {
    if (rank.has(g.id)) return rank.get(g.id);
    if (visiting.has(g.id)) return 0;
    visiting.add(g.id);
    const r = reads(g).reduce((m, id) => Math.max(m, visit(byId.get(id)) + 1), 0);
    visiting.delete(g.id);
    rank.set(g.id, r);
    return r;
  };
  groups.forEach(visit);
  return rank;
}

// Sizes of the drawing, in cytoscape pixels.
const MEMO_ITEM = { width: 190, height: 56, gap: 10, columns: 3, pad: 12, label: 34, rowGap: 70, groupGap: 36, rowWidth: 2200 };

// Step `s` of `memo` as cytoscape elements with preset positions. A group
// is node `g<id>` (a compound node holding its candidates, or a plain box
// when it has none, as at Stage 0); a candidate is `g<id>c<index>`; a
// collapsed group holds one node per origin, `g<id>o<origin>`. An edge goes
// from a candidate (or origin node, or a group without candidates) to each
// child group it reads. Options: `expanded` (ids of large groups drawn with
// every candidate), `collapseAbove` (default MEMO_COLLAPSE_ABOVE),
// `queryIds` (the workload's query ids). The selected plan's candidates
// are marked at the last step only; Pass 2's eliminated candidates are
// marked `pruned`.
function memoStepElements(memo, s, options) {
  const { expanded = [], collapseAbove = MEMO_COLLAPSE_ABOVE, queryIds = [] } = options || {};
  const step = memo.steps[s];
  const last = s === memo.steps.length - 1;
  const selected = last && memo.selected_candidates ? memo.selected_candidates : {};
  const queryName = (q) => queryIds[q] || `query #${q + 1}`;
  const ranks = memoGroupRanks(step.groups);
  const nodes = [];
  const edges = [];
  const edgeIds = new Set();
  const edge = (source, child, chosen, queries) => {
    const id = `${source}>g${child}`;
    if (edgeIds.has(id)) return;
    edgeIds.add(id);
    edges.push({ data: { id, source, target: `g${child}`, queries }, classes: chosen ? 'chosen' : '' });
  };

  // Each group's items (candidates or origin counts), laid out below.
  const boxes = step.groups.map((group) => {
    const chosenIndices = selected[String(group.id)] || [];
    const collapsible = group.candidates.length > collapseAbove;
    const collapsed = collapsible && !expanded.includes(group.id);
    const queries = group.queries;
    let items = [];
    if (collapsed) {
      const byOrigin = new Map();
      group.candidates.forEach((c) => {
        if (!byOrigin.has(c.origin)) byOrigin.set(c.origin, { added: 0, pruned: 0, children: new Set(), chosen: false, chosenChildren: [] });
        const entry = byOrigin.get(c.origin);
        if (c.added) entry.added += 1;
        if (c.pruned) entry.pruned += 1;
        c.children.forEach((child) => entry.children.add(child));
        if (chosenIndices.includes(c.index)) { entry.chosen = true; entry.chosenChildren.push(...c.children); }
      });
      items = Object.entries(group.candidate_count_by_origin).filter(([, count]) => count > 0).map(([origin, count]) => {
        const entry = byOrigin.get(origin);
        const lines = [`${count} ${MEMO_ORIGINS[origin].name}`];
        const notes = [entry.added ? `✚ ${entry.added} new` : '', entry.pruned ? `✗ ${entry.pruned} pruned` : '', entry.chosen ? '✓ selected' : ''].filter(Boolean);
        lines.push(notes.length ? notes.join(' · ') : 'click to expand');
        return {
          data: { id: `g${group.id}o${origin}`, kind: 'origin', groupId: group.id, origin, count, color: MEMO_ORIGINS[origin].color,
            label: lines.join('\n'), queries, w: MEMO_ITEM.width, h: MEMO_ITEM.height },
          classes: ['memoItem', entry.chosen ? 'chosen' : '', entry.added ? 'added' : ''].filter(Boolean).join(' '),
          children: [...entry.children], chosenChildren: entry.chosenChildren, chosen: entry.chosen,
        };
      });
    } else {
      items = group.candidates.map((c) => {
        const chosen = chosenIndices.includes(c.index);
        const notes = [MEMO_ORIGINS[c.origin].name, c.added ? '✚ new' : '', c.pruned ? '✗ pruned' : '', chosen ? '✓ selected' : ''].filter(Boolean);
        return {
          data: { id: `g${group.id}c${c.index}`, kind: 'candidate', groupId: group.id, index: c.index, origin: c.origin, color: MEMO_ORIGINS[c.origin].color,
            label: `${memoClip(`#${c.index} ${c.label}`, 30)}\n${memoClip(memoParametersText(c.parameters) || '—', 30)}\n${memoClip(notes.join(' · '), 30)}`, queries, w: MEMO_ITEM.width, h: MEMO_ITEM.height },
          classes: ['memoItem', chosen ? 'chosen' : '', c.added ? 'added' : '', c.pruned ? 'pruned' : ''].filter(Boolean).join(' '),
          children: c.children, chosenChildren: chosen ? c.children : [], chosen,
        };
      });
    }
    const pruned = group.candidates.filter((c) => c.pruned).length;
    const count = `${group.candidate_count} candidate${group.candidate_count === 1 ? '' : 's'}${pruned ? ` (${pruned} pruned by Pass 2)` : ''}`;
    const roots = group.roots_of.length ? ` · root of ${group.roots_of.map(queryName).join(', ')}` : '';
    const toggle = collapsible ? (collapsed ? ' · click a count to expand' : ' · click the group to collapse') : '';
    const label = `G${group.id} · ${memoClip(group.label, 40)}\n${step.step !== 'stage0' ? count : 'no candidates yet'}${roots}${toggle}`;
    const columns = Math.min(Math.max(items.length, 1), MEMO_ITEM.columns);
    const rows = Math.max(1, Math.ceil(items.length / MEMO_ITEM.columns));
    const width = items.length ? columns * MEMO_ITEM.width + (columns - 1) * MEMO_ITEM.gap + 2 * MEMO_ITEM.pad : 230;
    const height = items.length ? rows * MEMO_ITEM.height + (rows - 1) * MEMO_ITEM.gap + 2 * MEMO_ITEM.pad + MEMO_ITEM.label : 56;
    return { group, items, label, width, height, collapsible, collapsed, queries };
  });

  // Rank rows, highest rank on top; a rank wider than the row width wraps.
  // The row width grows with the drawing's area so that a step with
  // hundreds of groups (one tumbling-window-chain group per grid) stays roughly as
  // wide as it is tall, readable when the panel fits it.
  const area = boxes.reduce((n, b) => n + (b.width + MEMO_ITEM.groupGap) * (b.height + MEMO_ITEM.rowGap), 0);
  const rowWidth = Math.max(MEMO_ITEM.rowWidth, Math.sqrt(area) * 1.8);
  const byRank = new Map();
  boxes.forEach((box) => {
    const r = ranks.get(box.group.id);
    if (!byRank.has(r)) byRank.set(r, []);
    byRank.get(r).push(box);
  });
  let y = 0;
  [...byRank.keys()].sort((a, b) => b - a).forEach((r) => {
    const row = byRank.get(r).sort((a, b) => a.group.id - b.group.id);
    let line = [];
    const place = () => {
      if (!line.length) return;
      const width = line.reduce((n, b) => n + b.width, 0) + (line.length - 1) * MEMO_ITEM.groupGap;
      const height = Math.max(...line.map((b) => b.height));
      let x = -width / 2;
      line.forEach((b) => { b.x = x; b.y = y; x += b.width + MEMO_ITEM.groupGap; });
      y += height + MEMO_ITEM.rowGap;
      line = [];
    };
    let used = 0;
    row.forEach((box) => {
      if (line.length && used + box.width > rowWidth) { place(); used = 0; }
      line.push(box);
      used += box.width + MEMO_ITEM.groupGap;
    });
    place();
  });

  boxes.forEach((box) => {
    const { group, items } = box;
    const data = { id: `g${group.id}`, kind: 'group', groupId: group.id, label: box.label, queries: box.queries,
      collapsible: box.collapsible, collapsed: box.collapsed, w: box.width, h: box.height };
    const chosenGroup = Object.prototype.hasOwnProperty.call(selected, String(group.id));
    const node = { data, classes: ['memoGroup', items.length ? 'compound' : '', chosenGroup ? 'chosenGroup' : ''].filter(Boolean).join(' ') };
    if (!items.length) node.position = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
    nodes.push(node);
    items.forEach((item, k) => {
      const column = k % MEMO_ITEM.columns;
      const row = Math.floor(k / MEMO_ITEM.columns);
      nodes.push({
        data: Object.assign({ parent: `g${group.id}` }, item.data), classes: item.classes,
        position: {
          x: box.x + MEMO_ITEM.pad + column * (MEMO_ITEM.width + MEMO_ITEM.gap) + MEMO_ITEM.width / 2,
          y: box.y + MEMO_ITEM.label + MEMO_ITEM.pad + row * (MEMO_ITEM.height + MEMO_ITEM.gap) + MEMO_ITEM.height / 2,
        },
      });
      // Only the selected candidate's own edges are marked, not those of
      // the other candidates sharing its origin count.
      item.children.forEach((child) => edge(item.data.id, child, item.chosenChildren.includes(child), box.queries));
    });
    if (!items.length) group.children.forEach((child) => edge(`g${group.id}`, child, false, box.queries));
  });
  return nodes.concat(edges);
}

// The element ids of `elements` that workload query `query` (its index)
// does not reach: the groups whose `queries` leave it out, their items, and
// the edges touching any of them.
function memoQueryDimmedIds(elements, query) {
  const out = new Set();
  elements.forEach((e) => {
    if (!('source' in e.data) && !(e.data.queries || []).includes(query)) out.add(e.data.id);
  });
  elements.forEach((e) => {
    if ('source' in e.data && (out.has(e.data.source) || out.has(e.data.target))) out.add(e.data.id);
  });
  return [...out];
}

// The page part: draws the memo into #memoView (hidden without a memo),
// one tab per step in #memoTabs, details into #details. app.js calls
// `show(doc, queryIds)`, `highlightQuery(queryId)` and `restyle()`.
function memoViewController() {
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const tok = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  // The shown document and its memo section.
  let doc = null;
  let memo = null;
  let queryIds = [];
  let stepIndex = 0;
  let cy = null;
  let elements = [];
  let highlighted = '';
  // Per step, the large groups the user expanded.
  let expanded = [];

  function style() {
    return [
      { selector: 'node', style: {
          shape: 'round-rectangle', label: 'data(label)', 'text-wrap': 'wrap', 'text-valign': 'center', 'text-halign': 'center',
          'font-family': tok('--font-mono') || 'monospace', 'font-size': 10, color: tok('--fg'), 'text-max-width': 220,
          width: 'data(w)', height: 'data(h)', 'border-width': 1.5, 'background-color': tok('--panel'), 'border-color': tok('--line') } },
      { selector: 'node.memoGroup', style: { 'background-color': tok('--panel-2'), 'border-color': tok('--cat-rel-line'), 'font-family': tok('--font-body') || 'sans-serif', 'font-size': 11, 'text-max-width': 640 } },
      { selector: 'node.compound', style: { 'text-valign': 'top', 'text-halign': 'center', padding: 12 } },
      { selector: 'node.chosenGroup', style: { 'border-color': tok('--ok'), 'border-width': 2.5 } },
      { selector: 'node.memoItem', style: { 'border-color': 'data(color)', 'border-width': 3 } },
      { selector: 'node.added', style: { 'border-style': 'double', 'border-width': 5 } },
      { selector: 'node.chosen', style: { 'background-color': tok('--ok-soft') } },
      { selector: 'node.pruned', style: { 'background-color': tok('--bad-soft'), 'border-style': 'dashed' } },
      { selector: 'node:selected', style: { 'overlay-color': tok('--accent'), 'overlay-opacity': 0.14, 'overlay-padding': 4 } },
      { selector: 'edge', style: { width: 1.3, 'line-color': tok('--edge'), 'target-arrow-color': tok('--edge'), 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'arrow-scale': 0.8 } },
      { selector: 'edge.chosen', style: { width: 2.6, 'line-color': tok('--ok'), 'target-arrow-color': tok('--ok') } },
      { selector: '.dimmed', style: { opacity: 0.15 } },
    ];
  }

  function draw() {
    if (cy) { cy.destroy(); cy = null; }
    elements = memoStepElements(memo, stepIndex, { expanded: expanded[stepIndex], queryIds });
    cy = cytoscape({
      container: $('cyMemo'), elements, style: style(), layout: { name: 'preset', fit: true, padding: 16 },
      wheelSensitivity: 0.25, minZoom: 0.05, maxZoom: 2.5, boxSelectionEnabled: false,
    });
    cy.on('tap', 'node', (e) => tap(e.target.data()));
    cy.on('tap', (e) => { if (e.target === cy || typeof e.target.isNode !== 'function') $('details').innerHTML = HINT; });
    dim();
    const step = memo.steps[stepIndex];
    const selected = memo.selected_candidates;
    const last = stepIndex === memo.steps.length - 1;
    $('memoView-meta').innerHTML = `<b>${esc(step.title)}</b> · ${esc(memoStepText(memo, stepIndex))}`
      + (last ? (selected ? ` · selected plan's candidates highlighted (${Object.keys(selected).length} groups)` : ' · no plan selected')
        : ' · the selected plan\'s candidates are shown on the last tab');
    $('memoTabs').innerHTML = memo.steps.map((st, i) => `<button type="button" data-step="${i}" aria-pressed="${i === stepIndex}">${esc(st.title)}</button>`).join('');
    $('memoTabs').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => selectStep(Number(b.dataset.step))));
  }

  const HINT = '<p class="hint">Click a memo candidate to see its properties, its logical edges (Pass 2 tab) and its physical candidates; click a count to expand its group.</p>';

  function dim() {
    if (!cy || !cy.batch) return;
    const query = queryIds.indexOf(highlighted);
    const ids = new Set(highlighted && query >= 0 ? memoQueryDimmedIds(elements, query) : []);
    cy.batch(() => cy.elements().forEach((el) => el.toggleClass('dimmed', ids.has(el.id()))));
  }

  const groupOf = (id) => memo.steps[stepIndex].groups.find((g) => g.id === id);
  const kv = (items) => `<dl class="kv">${items.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('')}</dl>`;
  const groupList = (ids) => (ids.length ? ids.map((id) => `G${id}`).join(', ') : 'none');
  const queryList = (ids) => (ids.length ? ids.map((q) => queryIds[q] || `query #${q + 1}`).join(', ') : 'none');

  function tap(d) {
    const group = groupOf(d.groupId);
    if (d.kind === 'origin') { expanded[stepIndex] = [...expanded[stepIndex], d.groupId]; draw(); showGroup(group); return; }
    if (d.kind === 'group' && d.collapsible && !d.collapsed) { expanded[stepIndex] = expanded[stepIndex].filter((id) => id !== d.groupId); draw(); showGroup(group); return; }
    if (d.kind === 'candidate') showCandidate(group, group.candidates.find((c) => c.index === d.index));
    else showGroup(group);
  }

  const list = (items) => (items.length ? `<ul class="memo-own">${items.map((i) => `<li>${esc(i)}</li>`).join('')}</ul>` : '');

  // The selected plan's candidates of `group`, on the last step only.
  function selectedText(group) {
    const selected = memo.selected_candidates;
    if (stepIndex !== memo.steps.length - 1) return 'shown on the last tab';
    if (!selected) return 'no plan selected';
    const indices = selected[String(group.id)];
    if (!indices) return 'no candidate of this group is used';
    return indices.map((index) => {
      const c = group.candidates.find((x) => x.index === index);
      return `#${index} ${c.label} (${MEMO_ORIGINS[c.origin].name})`;
    }).join('; ');
  }

  // One line per candidate: label, θ, and whether Pass 2 pruned it.
  function candidateLine(c) {
    return [`#${c.index} ${c.label}`, memoParametersText(c.parameters), c.pruned ? 'pruned by Pass 2' : ''].filter(Boolean).join(' · ');
  }

  const section = (title, items) => (items.length ? `<div class="eyebrow">${esc(title)}</div>${list(items)}` : '');

  function showGroup(group) {
    const origins = Object.entries(group.candidate_count_by_origin).map(([o, n]) => `${n} ${MEMO_ORIGINS[o].name}`).join(', ');
    const pruned = memoPrunedLines(doc, group.id);
    $('details').innerHTML = `<h3>Memo group G${esc(group.id)} <span class="eyebrow">${esc(memo.steps[stepIndex].title)}</span></h3>${kv([
      ['label', group.label], ['operator', stageNodeLines(group.operator).join(' · ')],
      ['reads', groupList(group.children)], ['read by', groupList(group.parents)],
      ['queries reaching it', queryList(group.queries)], ['root of', queryList(group.roots_of)],
      ['candidates', `${group.candidate_count}${origins ? `: ${origins}` : ''}`],
      ['selected plan', selectedText(group)],
      ['pruned by Pass 2', `${((doc.stage1_logical_asap_optimization || {}).pruned_candidates || []).filter((p) => p.group === group.id).length || 'none'}`],
      ['pruned by Stage 2', pruned.stage2.length ? `${pruned.stage2.length}` : 'none'],
      ['pruned by Stage 3', pruned.stage3.length ? `${pruned.stage3.length}` : 'none'],
    ])}${list(group.candidates.map(candidateLine))}
      ${section('pruned by Pass 2', pruned.pass2)}${section('pruned by Stage 2', pruned.stage2)}${section('pruned by Stage 3', pruned.stage3)}
      <details><summary>Group operator (JSON)</summary><pre>${esc(JSON.stringify(group.operator, null, 2))}</pre></details>`;
  }

  // At the Pass 2 step (the memo Stage 2 expands): why Pass 2 pruned the
  // candidate, its logical edges, its physical candidates with their
  // physical edges, and the physical candidates Stage 2 and Stage 3 pruned.
  function showCandidate(group, candidate) {
    const indices = (memo.selected_candidates || {})[String(group.id)] || [];
    const selected = stepIndex === memo.steps.length - 1 && indices.includes(candidate.index);
    const pass2 = memo.steps[stepIndex].step === 'pass2';
    const edgeLists = (inputs) => inputs.map((input) => `<div class="eyebrow">${esc(input.title)}</div>${list(input.edges) || '<div class="hint">no edge</div>'}`).join('');
    const physical = pass2 ? memoPhysicalCandidates(doc, group.id, candidate.index) : [];
    const physicalHtml = physical.map((p) => `<div><b>${esc(p.title)}</b>${edgeLists(p.inputs)}</div>`).join('');
    const ofCandidate = (lines) => lines.filter((line) => line.startsWith(`#${candidate.index} `));
    const pruned = memoPrunedLines(doc, group.id);
    $('details').innerHTML = `<h3>Candidate #${esc(candidate.index)} of G${esc(group.id)} <span class="eyebrow">${esc(memo.steps[stepIndex].title)}</span></h3>${kv([
      ['label', candidate.label], ['origin', MEMO_ORIGINS[candidate.origin].name],
      ['added at this step', candidate.added ? 'yes' : 'no'],
      ['parameters θ', memoParametersText(candidate.parameters) || 'none'],
      ['required accuracy', candidate.children.length ? memoRequiredAccuracyTexts(candidate).join(', ') : 'no inputs'],
      ['reads', groupList(candidate.children)], ['fuses', groupList(candidate.fused_groups)],
      ['pruned by Pass 2', pass2 ? (candidate.pruned ? 'yes' : 'no') : 'shown on the Pass 2 tab'],
      ['selected plan', selected ? 'used by the selected plan' : 'no'],
    ])}${section('why Pass 2 pruned it', memoPass2Causes(doc, group.id, candidate.index))}
      ${pass2 && candidate.inputs.length ? `<div class="eyebrow">logical edges (α^log)</div>${edgeLists(memoLogicalEdges(candidate))}` : ''}
      <div class="eyebrow">physical candidates (Stage 2) and their edges (α^phys)</div>${physicalHtml || `<div class="hint">${pass2 ? 'none' : 'none (shown on the Pass 2 tab)'}</div>`}
      ${section('pruned by Stage 2', ofCandidate(pruned.stage2))}${section('pruned by Stage 3', ofCandidate(pruned.stage3))}
      <details><summary>Candidate operator (JSON)</summary><pre>${esc(JSON.stringify(candidate.operator, null, 2))}</pre></details>`;
  }

  function selectStep(i) {
    stepIndex = i;
    draw();
  }

  function show(shown, ids) {
    if (cy) { cy.destroy(); cy = null; }
    doc = shown;
    memo = shown.memo || null;
    queryIds = ids;
    highlighted = '';
    $('memoView').hidden = !memo;
    if (!memo) { $('memoTabs').innerHTML = ''; $('memoView-meta').textContent = ''; return; }
    expanded = memo.steps.map(() => []);
    // The last step first: it is the memo the selected plan was chosen from.
    selectStep(memo.steps.length - 1);
  }

  function highlightQuery(id) { highlighted = id; dim(); }
  function restyle() { if (cy) cy.style(style()); }

  return { show, highlightQuery, restyle, selectStep };
}

if (typeof window !== 'undefined' && typeof document !== 'undefined' && document.getElementById) window.MemoView = memoViewController();
