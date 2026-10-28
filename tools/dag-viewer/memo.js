// The Memo view of the Stage Viewer: the document's optional `memo` section
// (written by `stage_pipeline`) drawn after each memo step, one tab per
// step. Each memo group is a box holding its implementation candidates; an
// edge goes from a candidate to each child group it reads. The functions up
// to `memoViewController` are DOM-free (memo.test.mjs and test_viewer.py run
// them headless); the controller draws them on the page. `memo.enumeration`
// is not read here.
//
// The section, per step: `{step, title, groups: [{id, label, children,
// parents, queries, roots_of, dag, candidate_count,
// candidate_count_by_origin, candidates_capped, candidates: [{index,
// implements, children, origin, budget_share, added, label, dag,
// group_of_node}]}]}`; `selected_choice` maps a group id (string) of the
// last step to the selected plan's candidate index, or is null with
// `selected_choice_unavailable` saying why.

// Memo steps in pipeline order; a step production does not run is omitted.
const MEMO_STEPS = ['stage0', 'pass1', 'pass2', 'concrete_states'];

// `CandidateOrigin` (snake_case in the document), its Rust name, and the
// border color of its candidates (mid tones, readable in both themes).
const MEMO_ORIGINS = {
  pass1: { name: 'Pass1', color: '#4f7bd9' },
  shared_build: { name: 'SharedBuild', color: '#2e9e6b' },
  summary_result: { name: 'SummaryResult', color: '#a36ad6' },
  summary_input: { name: 'SummaryInput', color: '#d0842a' },
  panes: { name: 'Panes', color: '#d4524c' },
  rollup: { name: 'Rollup', color: '#1d9fb0' },
  top_k_reuse: { name: 'TopKReuse', color: '#b8931c' },
  widened_scans: { name: 'WidenedScans', color: '#848b98' },
};

// A group with more listed candidates than this is drawn collapsed: one
// node per origin with its count.
const MEMO_COLLAPSE_ABOVE = 12;

const memoIsIndexList = (ids) => Array.isArray(ids) && ids.every((id) => Number.isInteger(id) && id >= 0);

// A group's or candidate's `dag` (a flat logical DAG with its own roots):
// the stage DAG checks, with as many roots as it has.
function memoDagErrors(dag, where) {
  if (!dag || !Array.isArray(dag.nodes) || !Array.isArray(dag.roots)) return [`${where}: dag needs nodes and roots`];
  if (dag.nodes.length === 0) return [`${where}: dag has no nodes`];
  return validateLogicalDag(dag, where, dag.roots.length);
}

// The checks of `memo.steps` and `memo.selected_choice` against the format
// above, as messages ([] when it holds or the document has no memo).
// `queryCount` is the workload's query count.
function validateMemo(memo, queryCount) {
  if (memo === undefined) return [];
  if (!memo || typeof memo !== 'object' || Array.isArray(memo)) return ['memo must be an object'];
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
    step.groups.forEach((group, g) => {
      if (!group || !Number.isInteger(group.id) || group.id < 0) errors.push(`${at}.groups[${g}]: id must be a group id`);
      else if (ids.has(group.id)) errors.push(`${at}.groups[${g}]: duplicate group id ${group.id}`);
      else ids.add(group.id);
    });
    step.groups.forEach((group, g) => {
      if (!group || !ids.has(group.id)) return;
      const where = `${at} G${group.id}`;
      if (typeof group.label !== 'string') errors.push(`${where}: label must be a string`);
      const names = (list) => memoIsIndexList(list) && list.every((id) => ids.has(id));
      if (!names(group.children) || !names(group.parents)) errors.push(`${where}: children and parents must name groups of the step`);
      if (!memoIsIndexList(group.queries) || !memoIsIndexList(group.roots_of)
        || ![...group.queries, ...group.roots_of].every((q) => q < queryCount)) errors.push(`${where}: queries and roots_of must name workload queries`);
      errors.push(...memoDagErrors(group.dag, where));
      if (!Array.isArray(group.candidates)) { errors.push(`${where}: candidates must be an array`); return; }
      const count = group.candidate_count;
      if (!Number.isInteger(count) || count < group.candidates.length) errors.push(`${where}: candidate_count must count at least the listed candidates`);
      const byOrigin = group.candidate_count_by_origin;
      if (!byOrigin || typeof byOrigin !== 'object' || Object.keys(byOrigin).some((o) => !(o in MEMO_ORIGINS))
        || Object.values(byOrigin).reduce((n, c) => n + c, 0) !== count) errors.push(`${where}: candidate_count_by_origin must count candidate_count by known origin`);
      if (group.candidates_capped !== (group.candidates.length < count)) errors.push(`${where}: candidates_capped must say whether fewer than candidate_count are listed`);
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
        if (typeof candidate.label !== 'string' || typeof candidate.budget_share !== 'number') errors.push(`${cw}: needs a label and a numeric budget_share`);
        if (!names(candidate.children)) errors.push(`${cw}: children must name groups of the step`);
        if (typeof candidate.added !== 'boolean') errors.push(`${cw}: added must be a boolean`);
        errors.push(...memoDagErrors(candidate.dag, cw));
        const boundaries = candidate.group_of_node;
        const nodes = candidate.dag && Array.isArray(candidate.dag.nodes) ? candidate.dag.nodes.length : 0;
        if (!boundaries || typeof boundaries !== 'object' || Object.entries(boundaries).some(([node, child]) => !(/^\d+$/.test(node) && Number(node) < nodes)
          || !(Array.isArray(candidate.children) && candidate.children.includes(child)))) errors.push(`${cw}: group_of_node must map its dag's nodes to its child groups`);
      });
    });
  });
  if (errors.length) return errors;
  const choice = memo.selected_choice;
  if (choice === null || choice === undefined) {
    if (choice === null && typeof memo.selected_choice_unavailable !== 'string') errors.push('memo.selected_choice is null without selected_choice_unavailable');
    return errors;
  }
  if (typeof choice !== 'object' || Array.isArray(choice)) return ['memo.selected_choice must be an object or null'];
  const last = new Map(memo.steps[memo.steps.length - 1].groups.map((group) => [String(group.id), group]));
  Object.entries(choice).forEach(([group, index]) => {
    const g = last.get(group);
    if (!g) errors.push(`memo.selected_choice: ${group} is not a group of the last step`);
    else if (!Number.isInteger(index) || index < 0 || index >= g.candidate_count) errors.push(`memo.selected_choice: G${group} has no candidate ${index}`);
  });
  return errors;
}

// What step `s` changed: its groups, the groups it added and removed over
// step `s - 1` (all of them added at the first step), its candidates and
// their net change (`candidatesDelta`). A group's candidates are counted by
// `candidate_count`, listed or not; `listedAdded` counts the listed
// candidates marked `added`.
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
    listedAdded: groups.reduce((n, g) => n + g.candidates.filter((c) => c.added).length, 0),
  };
}

// The step's diff as one line.
function memoStepText(memo, s) {
  const d = memoStepDiff(memo, s);
  const plus = (n) => (s > 0 ? ` (${n >= 0 ? '+' : ''}${n})` : '');
  return `${d.groups} groups${plus(d.groupsAdded.length - d.groupsRemoved.length)} · ${d.candidates} candidates${plus(d.candidatesDelta)}`
    + (d.listedAdded ? ` · ${d.listedAdded} listed candidates new at this step` : '');
}

// `text` cut to `n` characters, with an ellipsis when cut.
function memoClip(text, n) {
  const s = String(text);
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}

// The node a FlatDag stands for: its first operator root, else its last node.
function memoDagTop(dag) {
  const root = dag.roots.find((r) => r && 'Operator' in r);
  return root ? root.Operator : dag.nodes.length - 1;
}

// A candidate's own nodes: its node and what it reads, down to (not
// through) the nodes `group_of_node` names as a child group's node. Returns
// `own` (node indices, top first, each with its display lines) and `reads`
// (the boundary nodes reached, with their group).
function memoCandidateOwnNodes(candidate) {
  const dag = candidate.dag;
  const boundaries = candidate.group_of_node || {};
  const own = [];
  const reads = [];
  const seen = new Set();
  const stack = [memoDagTop(dag)];
  while (stack.length) {
    const id = stack.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    if (String(id) in boundaries) { reads.push({ node: id, group: boundaries[String(id)] }); continue; }
    const node = dag.nodes[id];
    const [, body] = stageOperatorParts(node.operator);
    const children = stageLogicalChildren(body);
    const input = children.length ? dag.nodes[children[0]] : null;
    own.push({ node: id, lines: stageNodeLines(node.operator, input && input.schema) });
    children.slice().reverse().forEach((child) => stack.push(child));
  }
  reads.sort((a, b) => a.group - b.group);
  return { own, reads };
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
const MEMO_ITEM = { width: 190, height: 44, gap: 10, columns: 3, pad: 12, label: 34, rowGap: 70, groupGap: 36, rowWidth: 2200 };

// Step `s` of `memo` as cytoscape elements with preset positions. A group
// is node `g<id>` (a compound node holding its candidates, or a plain box
// when it has none, as at Stage 0); a candidate is `g<id>c<index>`; a
// collapsed group holds one node per origin, `g<id>o<origin>`. An edge goes
// from a candidate (or origin node, or a group without candidates) to each
// child group it reads. Options: `expanded` (ids of large groups drawn with
// every listed candidate), `collapseAbove` (default MEMO_COLLAPSE_ABOVE),
// `queryIds` (the workload's query ids). The selected choice is marked at
// the last step only.
function memoStepElements(memo, s, options) {
  const { expanded = [], collapseAbove = MEMO_COLLAPSE_ABOVE, queryIds = [] } = options || {};
  const step = memo.steps[s];
  const last = s === memo.steps.length - 1;
  const choice = last && memo.selected_choice ? memo.selected_choice : {};
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
    const chosenIndex = Object.prototype.hasOwnProperty.call(choice, String(group.id)) ? choice[String(group.id)] : null;
    const collapsible = group.candidates.length > collapseAbove;
    const collapsed = collapsible && !expanded.includes(group.id);
    const queries = group.queries;
    let items = [];
    if (collapsed) {
      const listed = new Map();
      group.candidates.forEach((c) => {
        if (!listed.has(c.origin)) listed.set(c.origin, { added: 0, children: new Set(), chosen: false, chosenChildren: [] });
        const entry = listed.get(c.origin);
        if (c.added) entry.added += 1;
        c.children.forEach((child) => entry.children.add(child));
        if (c.index === chosenIndex) { entry.chosen = true; entry.chosenChildren = c.children; }
      });
      items = Object.entries(group.candidate_count_by_origin).filter(([, count]) => count > 0).map(([origin, count]) => {
        const entry = listed.get(origin);
        const lines = [`${count} ${MEMO_ORIGINS[origin].name}`];
        if (!entry) {
          // The document lists a prefix of the group's candidates in memo
          // order; none of this origin is in it.
          lines.push('none listed');
          return {
            data: { id: `g${group.id}o${origin}`, kind: 'origin', groupId: group.id, origin, count, color: MEMO_ORIGINS[origin].color,
              label: lines.join('\n'), queries, w: MEMO_ITEM.width, h: MEMO_ITEM.height },
            classes: 'memoItem', children: [], chosenChildren: [], chosen: false,
          };
        }
        const notes = [entry.added ? `✚ ${entry.added} new` : '', entry.chosen ? '✓ selected' : ''].filter(Boolean);
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
        const chosen = c.index === chosenIndex;
        const notes = [MEMO_ORIGINS[c.origin].name, c.budget_share !== 1 ? `share ${c.budget_share}` : '', c.added ? '✚ new' : '', chosen ? '✓ selected' : ''].filter(Boolean);
        return {
          data: { id: `g${group.id}c${c.index}`, kind: 'candidate', groupId: group.id, index: c.index, origin: c.origin, color: MEMO_ORIGINS[c.origin].color,
            label: `${memoClip(`#${c.index} ${c.label}`, 30)}\n${memoClip(notes.join(' · '), 30)}`, queries, w: MEMO_ITEM.width, h: MEMO_ITEM.height },
          classes: ['memoItem', chosen ? 'chosen' : '', c.added ? 'added' : ''].filter(Boolean).join(' '),
          children: c.children, chosenChildren: chosen ? c.children : [], chosen,
        };
      });
    }
    const count = group.candidates_capped ? `${group.candidate_count} candidates (${group.candidates.length} listed)`
      : `${group.candidate_count} candidate${group.candidate_count === 1 ? '' : 's'}`;
    // The selected candidate may lie past the listed prefix.
    const unlisted = chosenIndex !== null && !group.candidates.some((c) => c.index === chosenIndex) ? ` · ✓ selected #${chosenIndex} (not listed)` : '';
    const roots = group.roots_of.length ? ` · root of ${group.roots_of.map(queryName).join(', ')}` : '';
    const toggle = collapsible ? (collapsed ? ' · click a count to expand' : ' · click the group to collapse') : '';
    const label = `G${group.id} · ${memoClip(group.label, 40)}\n${step.step !== 'stage0' ? count : 'no candidates yet'}${unlisted}${roots}${toggle}`;
    const columns = Math.min(Math.max(items.length, 1), MEMO_ITEM.columns);
    const rows = Math.max(1, Math.ceil(items.length / MEMO_ITEM.columns));
    const width = items.length ? columns * MEMO_ITEM.width + (columns - 1) * MEMO_ITEM.gap + 2 * MEMO_ITEM.pad : 230;
    const height = items.length ? rows * MEMO_ITEM.height + (rows - 1) * MEMO_ITEM.gap + 2 * MEMO_ITEM.pad + MEMO_ITEM.label : 56;
    return { group, items, label, width, height, collapsible, collapsed, queries };
  });

  // Rank rows, highest rank on top; a rank wider than rowWidth wraps.
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
      if (line.length && used + box.width > MEMO_ITEM.rowWidth) { place(); used = 0; }
      line.push(box);
      used += box.width + MEMO_ITEM.groupGap;
    });
    place();
  });

  boxes.forEach((box) => {
    const { group, items } = box;
    const data = { id: `g${group.id}`, kind: 'group', groupId: group.id, label: box.label, queries: box.queries,
      collapsible: box.collapsible, collapsed: box.collapsed, w: box.width, h: box.height };
    const chosenGroup = Object.prototype.hasOwnProperty.call(choice, String(group.id));
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
  let memo = null;
  let queryIds = [];
  let stepIndex = 0;
  let cy = null;
  let elements = [];
  let highlighted = '';
  // Per step, the large groups the reader expanded.
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
    const choice = memo.selected_choice;
    const last = stepIndex === memo.steps.length - 1;
    $('memoView-meta').innerHTML = `<b>${esc(step.title)}</b> · ${esc(memoStepText(memo, stepIndex))}`
      + (last ? (choice ? ` · selected plan's candidates highlighted (${Object.keys(choice).length} groups)` : ` · no selected choice: ${esc(memo.selected_choice_unavailable || 'none')}`)
        : ' · the selected choice is shown on the last tab');
    $('memoTabs').innerHTML = memo.steps.map((st, i) => `<button type="button" data-step="${i}" aria-pressed="${i === stepIndex}">${esc(st.title)}</button>`).join('');
    $('memoTabs').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => selectStep(Number(b.dataset.step))));
  }

  const HINT = '<p class="hint">Click a memo candidate to see its own nodes; click a count to expand its group.</p>';

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

  function nodeList(own) {
    return `<ol class="memo-own">${own.map((n) => `<li><code>node ${esc(n.node)}</code> ${esc(n.lines.join(' · '))}</li>`).join('')}</ol>`;
  }

  // The selected plan's candidate of `group`, on the last step only.
  function selectedText(group) {
    const choice = memo.selected_choice;
    if (stepIndex !== memo.steps.length - 1) return 'shown on the last tab';
    if (!choice) return `unavailable: ${memo.selected_choice_unavailable || 'none'}`;
    if (!Object.prototype.hasOwnProperty.call(choice, String(group.id))) return 'no candidate chosen in this group';
    const index = choice[String(group.id)];
    const c = group.candidates.find((x) => x.index === index);
    return c ? `#${index} ${c.label} (${MEMO_ORIGINS[c.origin].name})` : `#${index} (not listed)`;
  }

  function showGroup(group) {
    const top = memoDagTop(group.dag);
    const origins = Object.entries(group.candidate_count_by_origin).map(([o, n]) => `${n} ${MEMO_ORIGINS[o].name}`).join(', ');
    $('details').innerHTML = `<h3>Memo group G${esc(group.id)} <span class="eyebrow">${esc(memo.steps[stepIndex].title)}</span></h3>${kv([
      ['label', group.label], ['node', stageNodeLines(group.dag.nodes[top].operator).join(' · ')],
      ['reads', groupList(group.children)], ['read by', groupList(group.parents)],
      ['queries reaching it', queryList(group.queries)], ['root of', queryList(group.roots_of)],
      ['candidates', `${group.candidate_count}${group.candidates_capped ? ` (${group.candidates.length} listed)` : ''}${origins ? `: ${origins}` : ''}`],
      ['selected plan', selectedText(group)],
    ])}<details><summary>Group DAG (JSON)</summary><pre>${esc(JSON.stringify(group.dag, null, 2))}</pre></details>`;
  }

  function showCandidate(group, candidate) {
    const { own, reads } = memoCandidateOwnNodes(candidate);
    const choice = memo.selected_choice;
    const selected = stepIndex === memo.steps.length - 1 && choice && choice[String(group.id)] === candidate.index;
    $('details').innerHTML = `<h3>Candidate #${esc(candidate.index)} of G${esc(group.id)} <span class="eyebrow">${esc(memo.steps[stepIndex].title)}</span></h3>${kv([
      ['label', candidate.label], ['origin', MEMO_ORIGINS[candidate.origin].name],
      ['added at this step', candidate.added ? 'yes' : 'no'], ['budget share', candidate.budget_share],
      ['implements', groupList(candidate.implements || [])], ['reads', groupList(candidate.children)],
      ['selected plan', selected ? 'chosen for this group' : 'no'],
    ])}<div><div class="eyebrow">own nodes (top first)</div>${nodeList(own)}</div>
      <div class="hint">${reads.length ? `Reads ${reads.map((r) => `G${r.group} (node ${r.node})`).join(', ')}.` : 'Reads no other group.'}</div>
      <details><summary>Candidate DAG (JSON)</summary><pre>${esc(JSON.stringify(candidate.dag, null, 2))}</pre></details>`;
  }

  function selectStep(i) {
    stepIndex = i;
    draw();
  }

  function show(doc, ids) {
    if (cy) { cy.destroy(); cy = null; }
    memo = doc.memo || null;
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
