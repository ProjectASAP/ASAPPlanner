// ASAPPlanner Stage Viewer: an asap-stage-pipeline/v1 document as a Stage 3
// candidate list beside three DAG lanes stacked top to bottom (Stage 0,
// then the Stage 1 candidate, then its Stage 2 candidate), details below.
// Each lane shows the whole workload. Document checks, the candidate list
// and node labels live in stages.js (tested); node-style.js groups operator
// kinds.
(function () {
  if (window.cytoscapeDagre) cytoscape.use(window.cytoscapeDagre);
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const tok = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

  let doc, rows, queryIds, logicalById, physicalById;
  let current = { logical: null, physical: null };
  const cys = [null, null, null];
  const laneName = ['Stage 0 · query-language frontends', 'Stage 1 · logical ASAP optimization', 'Stage 2 · physical ASAP optimization'];

  function style() {
    return [
      { selector: 'node', style: {
          shape: 'round-rectangle', label: 'data(label)', 'text-wrap': 'wrap', 'text-valign': 'center', 'text-halign': 'center',
          'font-family': tok('--font-mono') || 'monospace', 'font-size': 10, color: tok('--fg'),
          width: 'label', height: 'label', padding: '9px', 'border-width': 1.5, 'text-max-width': 240 } },
      { selector: 'node[group = "source"]', style: { 'background-color': tok('--cat-source'), 'border-color': tok('--cat-source-line') } },
      { selector: 'node[group = "rel"]', style: { 'background-color': tok('--cat-rel'), 'border-color': tok('--cat-rel-line') } },
      { selector: 'node[group = "summary"]', style: { 'background-color': tok('--cat-summary'), 'border-color': tok('--cat-summary-line') } },
      { selector: 'node[group = "scalar"]', style: { 'background-color': tok('--cat-scalar'), 'border-color': tok('--cat-scalar-line'), 'border-style': 'dashed' } },
      // Stage 2: the fill says when a node runs (the border keeps its
      // category); the label's ⏱ line says it in words.
      { selector: 'node.ingestionTime', style: { 'background-color': tok('--time-ingest') } },
      { selector: 'node.queryTime', style: { 'background-color': tok('--time-query') } },
      { selector: 'node.root', style: { 'border-width': 3.5, 'border-color': tok('--accent') } },
      { selector: 'node:selected', style: { 'overlay-color': tok('--accent'), 'overlay-opacity': 0.14, 'overlay-padding': 5 } },
      { selector: 'edge', style: { width: 1.6, 'line-color': tok('--edge'), 'target-arrow-color': tok('--edge'), 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'arrow-scale': 0.9 } },
      { selector: '.dimmed', style: { opacity: 0.15 } },
      { selector: 'node.pane', style: { 'border-style': 'dashed' } },
      { selector: 'edge:selected', style: { 'line-color': tok('--accent'), 'target-arrow-color': tok('--accent'), width: 2.6 } },
    ];
  }

  function clearLane(i, text) {
    if (cys[i]) { cys[i].destroy(); cys[i] = null; }
    $(`lane${i}-meta`).textContent = text;
  }

  // Per lane: what it draws (to redraw it with a pane chain expanded or
  // collapsed) and its elements; per drawn DAG, the PaneChain node ids drawn
  // as panes, so switching plans and back keeps each plan's expanded chains.
  const drawn = [null, null, null];
  const laneElements = [[], [], []];
  const expandedByDag = new WeakMap();
  const expandedOf = (dag) => { if (!expandedByDag.has(dag)) expandedByDag.set(dag, new Set()); return expandedByDag.get(dag); };
  // The query whose path the lanes highlight ('' for none).
  let highlightedQuery = '';

  // `ids` names the queries of `dag.roots` in order (default: the workload's).
  function render(i, dag, physical, costPerNode, ids = queryIds) {
    if (cys[i]) cys[i].destroy();
    drawn[i] = { dag, physical, costPerNode, ids };
    const elements = stageLaneElements(dag, { physical, costPerNode, queryIds: ids, expandedChains: [...expandedOf(dag)] })
      .map((el) => ({ data: el.data, classes: [el.classes, el.data.rootFor && el.data.rootFor.length ? 'root' : ''].join(' ').trim() }));
    laneElements[i] = elements;
    const cy = cytoscape({
      container: $('cy' + i), elements, style: style(),
      layout: { name: window.cytoscapeDagre ? 'dagre' : 'breadthfirst', rankDir: 'BT', nodeSep: 18, rankSep: 34, padding: 14, directed: true },
      wheelSensitivity: 0.25, minZoom: 0.2, maxZoom: 2.5, boxSelectionEnabled: false,
    });
    cy.on('tap', 'node', (e) => tapNode(e.target.data(), i));
    cy.on('tap', 'edge', (e) => showEdge(e.target.data(), i));
    // A tap on the background clears a node's highlight back to the query's.
    cy.on('tap', (e) => { if (e.target === cy || typeof e.target.isNode !== 'function') dim(i, queryKeep(i)); });
    cys[i] = cy;
    dim(i, queryKeep(i));
  }

  function redraw(i) { const d = drawn[i]; render(i, d.dag, d.physical, d.costPerNode, d.ids); }

  // A PaneChain expands into its panes; a pane collapses its chain; any
  // other node is shown and its upstream and downstream highlighted.
  function tapNode(d, i) {
    if (d.expandable) { expandedOf(drawn[i].dag).add(d.nodeId); redraw(i); showNode(d, i); return; }
    if (d.kind === 'pane') { expandedOf(drawn[i].dag).delete(d.paneOf); redraw(i); return; }
    showNode(d, i);
    dim(i, new Set(stageNeighborhoodIds(laneElements[i], d.id)));
  }

  // The element ids lane `i` keeps bright for the highlighted query, or
  // null (nothing dimmed).
  function queryKeep(i) { return highlightedQuery ? new Set(stageQueryElementIds(laneElements[i], highlightedQuery)) : null; }

  // Dim every element of lane `i` not in `keep` (null: none).
  function dim(i, keep) {
    const cy = cys[i];
    if (!cy || !cy.batch) return;
    cy.batch(() => cy.elements().forEach((el) => el.toggleClass('dimmed', keep !== null && !keep.has(el.id()))));
  }

  function highlightQuery(id) {
    highlightedQuery = id;
    cys.forEach((cy, i) => dim(i, queryKeep(i)));
  }

  const fmtCost = (x) => (Math.abs(x) >= 100 ? x.toFixed(1) : String(Number(x.toPrecision(3))));
  const kv = (items) => `<dl class="kv">${items.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('')}</dl>`;

  function schemaTable(schema) {
    if (!schema || !Array.isArray(schema.fields)) return '';
    const fields = schema.fields.map((f) => `<tr><td>${esc(f.name)}</td><td>${esc(compactWire(f.dtype))}</td><td>${f.nullable ? 'yes' : 'no'}</td></tr>`).join('');
    return `<div><div class="eyebrow">output schema</div><div class="schema"><table><thead><tr><th>field</th><th>type</th><th>nullable</th></tr></thead><tbody>${fields}</tbody></table></div></div>`;
  }

  function showNode(d, lane) {
    const n = d.node;
    const items = [['lane', laneName[lane]], ['operator', d.kind], ['node id', d.nodeId]];
    if (d.kind === 'Scalar') items.push(['expression', stageScalarText(n.Scalar)]);
    const [, body] = d.kind === 'Scalar' ? [null, {}] : stageOperatorParts(n.payload || n.operator);
    if (d.kind === 'SummaryAgg') {
      items.push(['summary parameters', summaryFamilyText(body.family)]);
      items.push(['instances', summaryInstancesText(body.grouping, body.reduction)]);
    }
    if (d.rootFor && d.rootFor.length) items.push(['query root of', d.rootFor.join(', ')]);
    if (d.reachedBy) items.push(['queries reaching it', d.reachedBy.join(', ') || 'none']);
    if (d.paneOf !== undefined) items.push(['pane', `${d.pane + 1} of ${body.pane_count} of PaneChain ${d.paneOf}, window ${paneWindowText(d.pane, body.pane_width_ms)}`]);
    if (d.expandable) items.push(['panes', `${body.pane_count} × ${durationText(body.pane_width_ms)} (click the chain to show them, a pane to hide them)`]);
    if (d.timing !== undefined) items.push(['runs at', formatTiming(d.timing)]);
    if (n.materialization) items.push(['materialization', compactWire(n.materialization)]);
    if (n.output_state && n.output_state.primitive) items.push(['output primitive', compactWire(n.output_state.primitive)]);
    if (d.cost) {
      const value = typeof d.cost.cost_per_second === 'number'
        ? `${Number(d.cost.cost_per_second.toPrecision(4))}${d.cost.multiplier === 1 || d.cost.multiplier === undefined ? '' : ` × ${d.cost.multiplier}`}`
        : 'no number';
      items.push(['Stage 3 cost', `${value} · ${d.cost.detail || ''}`]);
    }
    if (d.kind !== 'Scalar') items.push(['guarantee', n.guarantee ? compactWire(n.guarantee) : 'none']);
    $('details').innerHTML = `<h3>${esc(d.kind)} <span class="eyebrow">node ${esc(d.nodeId)}</span></h3>${kv(items)}
      ${schemaTable(n.output_schema || n.schema)}
      <details><summary>Full node (JSON)</summary><pre>${esc(JSON.stringify(n, null, 2))}</pre></details>`;
  }

  function showEdge(d, lane) {
    const [from, to] = [d.source.slice(1), d.target.slice(1)];
    const items = [['lane', laneName[lane]], ['from → to', `node ${from} → node ${d.target.startsWith('s') ? `scalar root of query #${Number(to) + 1}` : to}`]];
    const e = d.edge;
    if (e) {
      items.push(['role', compactWire(e.role)], ['grouping', compactWire(e.grouping)]);
      if (e.data_state) items.push(['data state', compactWire(e.data_state)]);
      if (e.window) items.push(['window', compactWire(e.window)]);
    }
    $('details').innerHTML = `<h3>Edge <span class="eyebrow">${esc(from)} → ${esc(to)}</span></h3>${kv(items)}
      ${e ? schemaTable(e.intermediate_schema) : ''}`;
  }

  function statusChip(status) {
    if (status === 'selected') return '<span class="chip selected">✓ selected</span>';
    if (status === 'costlier') return '<span class="chip costlier">valid · costlier</span>';
    if (status === 'rejected') return '<span class="chip invalid">✗ invalid</span>';
    if (status === 'not judged') return '<span class="chip">not judged</span>';
    if (status === 'not kept') return '<span class="chip invalid">✗ invalid · not kept</span>';
    return `<span class="chip invalid">${esc(status)}</span>`;
  }

  function rowKey(row) {
    if (row.status === 'not kept') return `X:${row.id}`;
    return `${row.status === 'pruned' || row.status === 'composition error' ? 'L' : 'P'}:${row.id}`;
  }

  function renderRanking() {
    const pressed = current.physical ? `P:${current.physical}` : `L:${current.logical}`;
    $('rank').innerHTML = rows.map((r) => `<li><button type="button" data-key="${esc(rowKey(r))}" aria-pressed="${rowKey(r) === pressed}"${r.status === 'not kept' ? ' disabled title="the default search did not keep this candidate"' : ''}>
        <span class="pid">${esc(r.id)}</span><span class="plabel">${esc(r.display_name || r.id)}</span>
        <span class="ptotal">${r.total === null ? '—' : fmtCost(r.total)}</span>
        <span class="pstatus">${statusChip(r.status)}${r.reason ? `<span>${esc(r.reason)}</span>` : ''}${r.accepted ? `<span class="accepted">${esc(r.accepted)}</span>` : ''}</span></button></li>`).join('');
    $('rank').querySelectorAll('button:not([disabled])').forEach((b) => b.addEventListener('click', () => {
      const [kind, id] = [b.dataset.key.slice(0, 1), b.dataset.key.slice(2)];
      if (kind === 'P') selectPhysical(id);
      else selectLogical(id);
    }));
  }

  // Choose Stage 1 candidate `id`: show its physical plan when it has one;
  // a pruned or uncomposed candidate has none, so Stage 2 is cleared.
  function selectLogical(id) {
    pressEnumeratedRow(null);
    const physical = doc.stage2_physical_asap_optimization.candidates.find((p) => p.from_logical === id);
    if (physical) { selectPhysical(physical.id); return; }
    showLogical(id);
    current.physical = null;
    $('pick2').value = '';
    clearLane(2, 'no physical plan for this Stage 1 candidate');
    renderRanking();
  }

  // Draw Stage 1 candidate `id` in lane 1.
  function showLogical(id) {
    current.logical = id;
    $('pick1').value = id;
    const c = logicalById.get(id);
    if (c.dag) {
      render(1, c.dag, false, null);
      const physicalFor = doc.stage2_physical_asap_optimization.candidates.filter((p) => p.from_logical === id).map((p) => p.id);
      const pruned = (doc.stage2_physical_asap_optimization.pruned || []).find((p) => p.from_logical === id);
      $('lane1-meta').innerHTML = `${esc(c.display_name)} · ${c.dag.nodes.length} nodes · ` +
        (pruned ? `pruned by Stage 2: ${esc(pruned.reason)}` : `physical plan ${physicalFor.map((p) => `<b>${esc(p)}</b>`).join(', ') || 'none'}`);
    } else {
      clearLane(1, '');
      $('lane1-meta').innerHTML = `${esc(c.display_name)} · <span class="error">composition error: ${esc(c.error)}</span>`;
    }
  }

  // Draw enumerated DAG `id` of `memo.enumeration` (enumeration.js) in
  // lane 1; the document has no physical plan for it.
  function showEnumerated(id, dag, description, perQueryDag) {
    current = { logical: null, physical: null };
    $('pick1').value = '';
    $('pick2').value = '';
    // Its roots are its root groups; drawn per query when they map to queries.
    if (perQueryDag) render(1, perQueryDag, false, null);
    else render(1, dag, false, null, dag.roots.map((_, k) => `root group #${k + 1}`));
    $('lane1-meta').innerHTML = `enumerated DAG <b>${esc(id)}</b> · ${esc(description)} · ${dag.nodes.length} nodes` +
      (perQueryDag ? '' : ' · roots not matched to queries');
    clearLane(2, 'no physical plan for an enumerated DAG');
    renderRanking();
  }

  function selectPhysical(id) {
    pressEnumeratedRow(null);
    current.physical = id;
    $('pick2').value = id;
    const c = physicalById.get(id);
    const selection = doc.stage3_plan_selection;
    const cost = selection.costs[id];
    render(2, c.dag, true, cost ? cost.per_node : null);
    const row = rows.find((r) => rowKey(r) === `P:${id}`);
    const priced = rows.filter((r) => r.rank !== null).length;
    $('lane2-meta').innerHTML = `${esc(c.display_name)} · from <b>${esc(c.from_logical)}</b> · ` +
      (cost && cost.cost_per_second !== null ? `total <b>${fmtCost(cost.cost_per_second)}</b> ${esc(cost.cost_unit.replace(/_/g, ' '))} · rank ${row.rank} of ${priced}`
        : cost ? 'no price (cost model 3)' : 'not priced') +
      ` · ${statusChip(row.status)}` +
      (row.accepted ? ` · <span class="accepted">${esc(row.accepted)}</span>` : '') +
      (cost && cost.usage ? `<br>Stage 3 usage: ${esc(usageText(cost.usage))}` : '');
    if (c.from_logical !== current.logical) showLogical(c.from_logical);
    renderRanking();
  }

  function restyle() { cys.forEach((cy) => cy && cy.style(style())); }

  const HINT = '<p class="hint">Select a node to see its operator, output schema, timing and Stage 3 cost; select an edge to see the schema and data state it carries.</p>';

  // Show one stage document; `source` names it, `story` explains it.
  function showDocument(data, source, story) {
    const errors = validateStagePipeline(data).concat(validateMemoEnumeration(data));
    if (errors.length) throw new Error(errors.slice(0, 3).join('; '));
    doc = data;
    current = { logical: null, physical: null };
    $('details').innerHTML = HINT;
    $('docName').textContent = source || '';
    $('story').textContent = story || '';
    $('story').hidden = !story;
    queryIds = doc.workload.queries.map((q) => q.id);
    highlightedQuery = '';
    $('queryPick').innerHTML = '<option value="">all queries</option>' + queryIds.map((q) => `<option value="${esc(q)}">${esc(q)}</option>`).join('');
    $('queryPick').value = '';
    const logical = doc.stage1_logical_asap_optimization;
    const physical = doc.stage2_physical_asap_optimization;
    const selection = doc.stage3_plan_selection;
    logicalById = new Map(logical.candidates.map((c) => [c.id, c]));
    physicalById = new Map(physical.candidates.map((c) => [c.id, c]));
    rows = stageCandidateRows(doc);

    $('queries').innerHTML = doc.workload.queries.map((q) => {
      const r = q.requirements || {};
      const acc = accuracyText(r.accuracy, r.default_metric);
      const schedule = queryScheduleText(q) || (r.repeat_interval_ms && `every ${r.repeat_interval_ms / 1000} s`);
      const req = [acc && `accuracy ${acc}`, r.max_latency_ms && `latency ≤ ${r.max_latency_ms} ms`, schedule].filter(Boolean).join(' · ');
      return `<div class="query"><span class="qid">${esc(q.id)} · ${esc(q.language)}</span><code>${esc(q.text)}</code>${req ? `<span class="req">${esc(req)}</span>` : ''}</div>`;
    }).join('');
    const count = (status) => rows.filter((r) => r.status === status).length;
    $('counts').innerHTML = `Candidates per stage: <b>1</b> logical DAG → <b>${logical.combinations}</b> logical ASAP` +
      (logical.capped ? ` (<b>${logical.candidates.length}</b> shown)` : '') +
      ` → <b>${physicalById.size}</b> physical` +
      (selection.selected ? ` → <b>1</b> selected (${count('rejected')} invalid, ${count('costlier')} valid but costlier)` : ' → none selected');
    $('rank-meta').textContent = [stageCandidateHeader(doc), accuracyGuaranteesText(doc), searchText(doc)].filter(Boolean).join(' · ');
    renderDataWorkload();
    const sel = selection.selected ? selection.costs[selection.selected] : null;
    $('cost-unit').textContent = sel ? sel.cost_unit.replace(/_/g, ' ') : '';

    $('pick1').innerHTML = logical.candidates.map((c) => `<option value="${esc(c.id)}">${esc(c.id)} · ${esc(c.display_name)}</option>`).join('');
    $('pick2').innerHTML = rows.filter((r) => rowKey(r).startsWith('P'))
      .sort((a, b) => byPlanNumber(a.id, b.id))
      .map((r) => `<option value="${esc(r.id)}">${esc(r.id)} · ${esc(r.display_name)}</option>`).join('');

    render(0, doc.stage0_query_language_frontends.dag, false, null);
    $('lane0-meta').innerHTML = `${doc.stage0_query_language_frontends.dag.nodes.length} nodes · roots for ${queryIds.map((q) => `<b>${esc(q)}</b>`).join(', ')}`;
    // The selected plan is the first row when there is one.
    const first = rows[0];
    if (first && rowKey(first).startsWith('P')) selectPhysical(first.id);
    else if (first && first.logical) selectLogical(first.logical);
    else {
      clearLane(1, 'no candidates');
      clearLane(2, 'no candidates');
      renderRanking();
    }
    renderMaterializedViews();
    renderMemoSearch();
    renderEnumeration(doc, showEnumerated);
  }

  const table = (head, body) => `<table><thead><tr>${head.map((h) => `<th>${esc(h)}</th>`).join('')}</tr></thead><tbody>${body}</tbody></table>`;

  // The run's data workload beside the queries: one row per source with the
  // facts it declares, and the queries whose text names it.
  function renderDataWorkload() {
    const dataWorkload = doc.workload.data_workload;
    if (!dataWorkload) {
      $('dataArrival').textContent = '';
      $('deployment').textContent = doc.deployment_capabilities ? `Deployment: ${deploymentText(doc.deployment_capabilities)}` : '';
      $('dataSources').innerHTML = '<span class="status">The document declares no data workload.</span>';
      return;
    }
    const view = dataWorkloadView(dataWorkload, doc.workload.queries);
    $('dataArrival').textContent = `arrival: ${view.arrival}`;
    $('deployment').textContent = doc.deployment_capabilities ? `Deployment: ${deploymentText(doc.deployment_capabilities)}` : '';
    $('dataSources').innerHTML = view.rows.length
      ? table(['source', 'series', 'type', 'non-negative', 'sample interval', 'stream norms', 'distinct bound', 'distinct values per label', 'distinct values per label set', 'read by'],
        view.rows.map((r) => `<tr><td>${esc(r.name)}</td><td>${esc(r.series)}</td><td>${esc(r.type)}</td><td>${esc(r.nonNegative)}</td><td>${esc(r.sampleInterval)}</td><td>${esc(r.streamNorms)}</td><td>${esc(r.maxDistinct)}</td><td class="wrap">${esc(r.distinctValuesPerLabel)}</td><td class="wrap">${esc(r.distinctValuesPerLabelSet)}</td><td class="wrap">${esc(r.readBy.join(', ') || '—')}</td></tr>`).join(''))
      : '<span class="status">No source declared.</span>';
  }

  // The selected plan's materialized views, one row per Stage 2 node (a pane
  // chain's panes listed inside its row); a row selects its Stage 2 node.
  function renderMaterializedViews() {
    const selection = doc.stage3_plan_selection;
    if (!selection.selected) {
      $('views-meta').textContent = 'no plan selected';
      $('materializedViews').innerHTML = '';
      return;
    }
    if (selection.materialized_views === undefined) {
      $('views-meta').textContent = 'this document has no materialized views section; write it again with the current stage_pipeline';
      $('materializedViews').innerHTML = '';
      return;
    }
    const groups = materializedViewGroups(doc);
    const count = groups.reduce((n, g) => n + g.panes.length, 0);
    $('views-meta').innerHTML = selection.materialized_views_error
      ? `<span class="error">no installable plan: ${esc(selection.materialized_views_error)}</span>`
      : `<b>${count}</b> materialized view${count === 1 ? '' : 's'} in <b>${groups.length}</b> group${groups.length === 1 ? '' : 's'} of <b>${esc(selection.selected)}</b>` +
        (count ? '' : ': nothing is kept across evaluations; every query computes at query time');
    const panes = (g) => (g.panes.length > 1
      ? `<details><summary>${esc(g.panes.length)} installed nodes</summary><ul>${g.panes.map((p) => `<li>node ${esc(p.nodeId)} · ${esc(p.pane)} · window ${esc(p.window)} · kept ${esc(p.retention)}</li>`).join('')}</ul></details>`
      : `node ${esc(g.panes[0].nodeId)} · window ${esc(g.panes[0].window)}`);
    $('materializedViews').innerHTML = groups.length ? table(
      ['Stage 2 node', 'definition', 'kind', 'panes', 'refresh', 'retention', 'installed', 'Stage 3 cost', 'read by'],
      groups.map((g) => `<tr class="clickable" tabindex="0" data-node="${g.stage2NodeId === null ? '' : esc(g.stage2NodeId)}"><td>${g.stage2NodeId === null ? '—' : esc(g.stage2NodeId)}</td>` +
        `<td class="wrap">${esc(g.definition)}</td><td class="wrap">${esc(g.kind)}</td><td>${esc(g.panesText)}</td><td>${esc(g.refresh)}</td>` +
        `<td>${esc(g.retention)}</td><td class="wrap">${panes(g)}</td><td class="wrap">${esc(g.cost)}</td><td class="wrap">${esc(g.readBy.join(', ') || '—')}</td></tr>`).join('')) : '';
    $('materializedViews').querySelectorAll('tr.clickable').forEach((tr) => {
      const pick = (e) => { if (e && e.target && e.target.closest && e.target.closest('details')) return; if (tr.dataset.node !== '') selectStage2Node(Number(tr.dataset.node)); };
      tr.addEventListener('click', pick);
      tr.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); pick(); } });
    });
  }

  // Joint-search metrics; historical documents may additionally list shared units.
  function renderMemoSearch() {
    const view = memoSearchView(doc);
    $('memoPanel').hidden = !view;
    if (!view) return;
    $('memo-meta').textContent = [memoSummaryText(view.summary),
      view.scansWidened ? 'selected plan: the scan-widening variant of the solution' : ''].filter(Boolean).join(' · ');
    $('memoShared').innerHTML = view.sharedUnits.length
      ? table(['shared unit', 'kind', 'readers', 'queries', 'threshold', 'runs', 'strictest reader'],
        view.sharedUnits.map((u) => `<tr><td class="wrap">${esc(u.unit)}</td><td>${esc(u.kind)}</td><td class="wrap">${esc(u.readers.join(', '))}</td><td>${esc(u.queries.join(', '))}</td>` +
          `<td class="wrap">${esc(u.threshold)}</td><td class="wrap">${esc(u.schedule)}</td><td>${esc(u.strictest)}</td></tr>`).join(''))
      : (view.summary && view.summary.status
        ? '<span class="status">Inspect the selected execution DAG for shared producer nodes.</span>'
        : '<span class="status">No shared units recorded.</span>');
  }

  // Show the selected plan in the Stage 2 lane and select its node `id`.
  function selectStage2Node(id) {
    const selected = doc.stage3_plan_selection.selected;
    if (current.physical !== selected) selectPhysical(selected);
    const cy = cys[2];
    if (!cy) return;
    // An expanded pane chain is drawn as its panes: select the newest.
    let node = cy.getElementById(`n${id}`);
    if (!node || node.length === 0) node = cy.getElementById(`n${id}p0`);
    if (!node || node.length === 0) return;
    cy.elements().unselect();
    node.select();
    cy.animate ? cy.animate({ center: { eles: node }, duration: 200 }) : cy.center(node);
    showNode(node.data(), 2);
  }

  function showError(err) {
    $('queries').innerHTML = `<span class="error">Could not load the planner output: ${esc(err.message)}</span>`;
  }

  // Loads are numbered so a slow earlier fetch cannot replace a later one.
  let loading = 0;
  function loadUrl(url, source, story) {
    const ticket = ++loading;
    $('queries').innerHTML = '<span class="status">Loading planner output…</span>';
    return fetch(url).then((r) => { if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`); return r.json(); })
      .then((data) => { if (ticket === loading) showDocument(data, source, story); })
      .catch((err) => { if (ticket === loading) showError(err); });
  }

  let examples = [];
  function pressTab(id) {
    $('tabs').querySelectorAll('button').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.id === id)));
  }
  function loadExample(example) {
    pressTab(example.id);
    try { history.replaceState(null, '', '#' + example.id); } catch (e) { /* the hash is a convenience */ }
    loadUrl(example.file, example.file, example.story);
  }

  // Documents from outside the example list (a file, the editor) clear the
  // tab. Returns whether the document was shown.
  function showExternal(data, source) {
    ++loading;
    pressTab(null);
    try { history.replaceState(null, '', location.pathname + location.search); } catch (e) { /* ignore */ }
    try {
      showDocument(data, source, '');
      return true;
    } catch (err) {
      showError(err);
      return false;
    }
  }

  // Plan a `PlanningWorkload` JSON with the local server (`stage_pipeline
  // --workload`, default search) under `deploymentCapabilities` and
  // `costModel` (null: the defaults), with accuracy guarantees ignored
  // unless `accuracyGuarantees` is "enforced", and the SQL `tables` it reads
  // (`--table` declarations; null for none), and show the document.
  // Resolves to whether it was shown; rejects when the server could not plan it.
  function planWorkload(workload, deploymentCapabilities, source, story, costModel, accuracyGuarantees, tables) {
    const ticket = ++loading;
    pressTab(null);
    $('queries').innerHTML = `<span class="status">Planning ${esc(source)} with the local server…</span>`;
    return fetch('/api/plan-workload', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ workload, tables: tables || null, deployment_capabilities: deploymentCapabilities || null, cost_model: costModel || null, accuracy_guarantees: accuracyGuarantees || null }),
    }).then((r) => r.json().catch(() => ({ error: `HTTP ${r.status}` })).then((result) => {
      if (!r.ok) throw new Error(result.error || `HTTP ${r.status}`);
      if (ticket !== loading) return false;
      try { history.replaceState(null, '', location.pathname + location.search); } catch (e) { /* ignore */ }
      showDocument(result, source, story || '');
      return true;
    })).catch((err) => {
      if (ticket === loading) $('queries').innerHTML = `<span class="error">Could not plan ${esc(source)}: ${esc(err.message)}</span>`;
      throw err;
    });
  }

  // A dropped or opened file: a stage document is shown, a workload planned.
  function readFile(file) {
    file.text().then((text) => {
      const value = JSON.parse(text);
      if (isPlanningWorkload(value)) planWorkload(value, null, file.name, '').catch(() => {});
      else showExternal(value, file.name);
    }).catch(showError);
  }
  $('openFile').addEventListener('click', () => $('fileInput').click());
  $('fileInput').addEventListener('change', (e) => { if (e.target.files[0]) readFile(e.target.files[0]); e.target.value = ''; });
  document.addEventListener('dragover', (e) => { e.preventDefault(); document.body.classList.add('drop-active'); });
  document.addEventListener('dragleave', (e) => { if (!e.relatedTarget) document.body.classList.remove('drop-active'); });
  document.addEventListener('drop', (e) => {
    e.preventDefault();
    document.body.classList.remove('drop-active');
    if (e.dataTransfer.files[0]) readFile(e.dataTransfer.files[0]);
  });

  $('pick1').addEventListener('change', (e) => selectLogical(e.target.value));
  $('queryPick').addEventListener('change', (e) => highlightQuery(e.target.value));
  $('pick2').addEventListener('change', (e) => selectPhysical(e.target.value));
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', restyle);
  new MutationObserver(restyle).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  $('details').innerHTML = HINT;

  window.StageViewer = {
    showDocument: showExternal,
    planWorkload,
    loadDocument: (url, source, story) => { pressTab(null); return loadUrl(url, source, story); },
  };

  // `?doc=<path>` opens one document; otherwise the examples listed in
  // examples.json, starting from `#<id>` or the first.
  const requested = new URLSearchParams(location.search).get('doc');
  fetch('examples.json').then((r) => (r.ok ? r.json() : [])).catch(() => []).then((list) => {
    examples = Array.isArray(list) ? list : [];
    $('tabs').innerHTML = examples.map((x) => `<button type="button" data-id="${esc(x.id)}" aria-pressed="false">${esc(x.name)}</button>`).join('');
    $('tabs').hidden = examples.length === 0;
    $('tabs').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => loadExample(examples.find((x) => x.id === b.dataset.id))));
    if (requested) loadUrl(requested, requested, '');
    else if (examples.length) loadExample(examples.find((x) => '#' + x.id === location.hash) || examples[0]);
  });
})();
