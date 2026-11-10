// ASAPPlanner Stage Viewer: an asap-stage-pipeline/v2 document as Stage 3's
// outcome (the selected plan or its error) beside two DAG lanes stacked top
// to bottom (Stage 0, then Stage 3's plan), details below. Each lane shows
// the whole workload. Document checks, the plan row and node labels live in
// stages.js (tested); node-style.js groups operator kinds.
(function () {
  if (window.cytoscapeDagre) cytoscape.use(window.cytoscapeDagre);
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const tok = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

  let doc, queryIds;
  const cys = [null, null];
  const laneName = ['Stage 0 · query-language frontends', 'Stage 3 · selected plan'];

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
      // The plan: the fill says when a node runs (the border keeps its
      // category); the label's ⏱ line says it in words.
      { selector: 'node.ingestionTime', style: { 'background-color': tok('--time-ingest') } },
      { selector: 'node.queryTime', style: { 'background-color': tok('--time-query') } },
      { selector: 'node.root', style: { 'border-width': 3.5, 'border-color': tok('--accent') } },
      { selector: 'node:selected', style: { 'overlay-color': tok('--accent'), 'overlay-opacity': 0.14, 'overlay-padding': 5 } },
      { selector: 'edge', style: { width: 1.6, 'line-color': tok('--edge'), 'target-arrow-color': tok('--edge'), 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'arrow-scale': 0.9 } },
      { selector: '.dimmed', style: { opacity: 0.15 } },
      { selector: 'edge:selected', style: { 'line-color': tok('--accent'), 'target-arrow-color': tok('--accent'), width: 2.6 } },
    ];
  }

  function clearLane(i, text) {
    if (cys[i]) { cys[i].destroy(); cys[i] = null; }
    laneElements[i] = [];
    $(`lane${i}-meta`).textContent = text;
  }

  // Per lane, its elements, for highlighting.
  const laneElements = [[], []];
  // The query whose path the lanes highlight ('' for none).
  let highlightedQuery = '';

  function render(i, dag, physical, costPerNode) {
    if (cys[i]) cys[i].destroy();
    const elements = stageLaneElements(dag, { physical, costPerNode, queryIds })
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
    fitGraph(cy);
    dim(i, queryKeep(i));
  }

  // A tapped node is shown and its upstream and downstream highlighted.
  function tapNode(d, i) {
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
    if (window.MemoView) window.MemoView.highlightQuery(id);
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
    if ('summary_type' in body) items.push(['summary type', summaryTypeText(body.summary_type)]);
    if (d.kind === 'SummaryTumblingWindowChain') items.push(['tumbling window width', durationText(body.tumbling_window_width_ms)]);
    if (d.rootFor && d.rootFor.length) items.push(['query root of', d.rootFor.join(', ')]);
    if (d.reachedBy) items.push(['queries reaching it', d.reachedBy.join(', ') || 'none']);
    if (d.timing !== undefined) items.push(['runs at', formatTiming(d.timing)]);
    if (n.lifetime) items.push(['lifetime', compactWire(n.lifetime)]);
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

  // The plan's cost per second as text.
  function costText(cost) {
    if (!cost) return 'not costed';
    return cost.cost_per_second === null ? 'no cost (cost model 3)' : `cost <b>${fmtCost(cost.cost_per_second)}</b> per second`;
  }

  // Draw Stage 3's plan in lane 1 with its node costs, or say why there is none.
  function renderPlan() {
    const stage3 = doc.stage3_plan_selection;
    if (!stage3.plan) {
      clearLane(1, stage3.error ? `no plan: ${stage3.error}` : 'Stage 3 did not run');
      return;
    }
    render(1, stage3.plan, true, stage3.cost ? stage3.cost.per_node : null);
    const accepted = acceptedText(stage3.accepted_accuracy_per_query);
    $('lane1-meta').innerHTML = `${stage3.plan.nodes.length} nodes · ${costText(stage3.cost)}` +
      (accepted ? ` · <span class="accepted">${esc(accepted)}</span>` : '') +
      (stage3.cost && stage3.cost.usage ? `<br>Stage 3 usage: ${esc(usageText(stage3.cost.usage))}` : '');
  }

  function restyle() { cys.forEach((cy) => cy && cy.style(style())); if (window.MemoView) window.MemoView.restyle(); }

  const HINT = '<p class="hint">Select a node to see its operator, output schema, timing and Stage 3 cost; select an edge to see the schema and data state it carries.</p>';

  // Show one stage document; `source` names it, `story` explains it.
  function showDocument(data, source, story) {
    fillStagesNotRun(data);
    const errors = validateStagePipeline(data);
    if (!errors.length && typeof validateMemo === 'function') errors.push(...validateMemo(data.memo, data.workload.queries.length));
    if (errors.length) throw new Error(errors.slice(0, 3).join('; '));
    doc = data;
    $('details').innerHTML = HINT;
    $('docName').textContent = source || '';
    $('story').textContent = story || '';
    $('story').hidden = !story;
    queryIds = doc.workload.queries.map((q) => q.id);
    highlightedQuery = '';
    $('queryPick').innerHTML = '<option value="">all queries</option>' + queryIds.map((q) => `<option value="${esc(q)}">${esc(q)}</option>`).join('');
    $('queryPick').value = '';

    $('queries').innerHTML = doc.workload.queries.map((q) => {
      const r = q.requirements || {};
      const acc = accuracyText(r.accuracy, r.default_metric);
      const schedule = queryScheduleText(q) || (r.repeat_interval_ms && `every ${r.repeat_interval_ms / 1000} s`);
      const req = [acc && `accuracy ${acc}`, r.max_latency_ms && `latency ≤ ${r.max_latency_ms} ms`, schedule].filter(Boolean).join(' · ');
      return `<div class="query"><span class="qid">${esc(q.id)} · ${esc(q.language)}</span><code>${esc(q.text)}</code>${req ? `<span class="req">${esc(req)}</span>` : ''}</div>`;
    }).join('');
    $('counts').textContent = candidateCountsText(doc);
    // Stage 3's outcome, failures and warnings, and how it searched.
    const stage3 = doc.stage3_plan_selection;
    $('outcome').textContent = [stage3.plan ? 'plan selected' : `no plan selected${stage3.error ? `: ${stage3.error}` : ''}`,
      planHeader(doc), searchText(doc)].filter(Boolean).join(' · ');
    renderDataWorkload();

    render(0, doc.stage0_query_language_frontends.dag, false, null);
    $('lane0-meta').innerHTML = `${doc.stage0_query_language_frontends.dag.nodes.length} nodes · roots for ${queryIds.map((q) => `<b>${esc(q)}</b>`).join(', ')}`;
    renderPlan();
    renderMaterializedViews();
    if (window.MemoView) window.MemoView.show(doc, queryIds);
  }

  const table = (head, body) => `<table><thead><tr>${head.map((h) => `<th>${esc(h)}</th>`).join('')}</tr></thead><tbody>${body}</tbody></table>`;

  // The run's data workload beside the queries: one row per source with the
  // facts it declares, and the queries whose text names it.
  function renderDataWorkload() {
    $('deployment').innerHTML = settingsRows(doc).map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('');
    const dataWorkload = doc.workload.data_workload;
    if (!dataWorkload) {
      $('dataArrival').textContent = '';
      $('dataSources').innerHTML = '<span class="status">The document declares no data workload.</span>';
      return;
    }
    const view = dataWorkloadView(dataWorkload, doc.workload.queries);
    $('dataArrival').textContent = `arrival: ${view.arrival}`;
    $('dataSources').innerHTML = view.rows.length
      ? table(['source', 'series', 'type', 'non-negative', 'sample interval', 'stream norms', 'distinct bound', 'distinct values per label', 'distinct values per label set', 'read by'],
        view.rows.map((r) => `<tr><td>${esc(r.name)}</td><td>${esc(r.series)}</td><td>${esc(r.type)}</td><td>${esc(r.nonNegative)}</td><td>${esc(r.sampleInterval)}</td><td>${esc(r.streamNorms)}</td><td>${esc(r.maxDistinct)}</td><td class="wrap">${esc(r.distinctValuesPerLabel)}</td><td class="wrap">${esc(r.distinctValuesPerLabelSet)}</td><td class="wrap">${esc(r.readBy.join(', ') || '—')}</td></tr>`).join(''))
      : '<span class="status">No source declared.</span>';
  }

  // The plan's materialized views, one row per view; a row selects its plan node.
  function renderMaterializedViews() {
    const stage3 = doc.stage3_plan_selection;
    if (!stage3.plan) {
      $('views-meta').textContent = 'no plan selected';
      $('materializedViews').innerHTML = '';
      return;
    }
    const rows = materializedViewRows(doc);
    $('views-meta').innerHTML = stage3.materialized_views_error
      ? `<span class="error">no installable plan: ${esc(stage3.materialized_views_error)}</span>`
      : `<b>${rows.length}</b> materialized view${rows.length === 1 ? '' : 's'}` +
        (rows.length ? '' : ': nothing is materialized across evaluations; every query computes at query time');
    $('materializedViews').innerHTML = rows.length ? table(
      ['plan node', 'definition', 'kind', 'refresh', 'reads', 'retention', 'raw-data windows', 'Stage 3 cost', 'read by'],
      rows.map((r) => `<tr class="clickable" tabindex="0" data-node="${esc(r.nodeId)}"><td>${esc(r.nodeId)}</td>` +
        `<td class="wrap">${esc(r.definition)}</td><td class="wrap">${esc(r.kind)}</td><td>${esc(r.refresh)}</td><td class="wrap">${esc(r.inputs)}</td>` +
        `<td>${esc(r.retention)}</td><td class="wrap">${esc(r.window)}</td><td class="wrap">${esc(r.cost)}</td><td class="wrap">${esc(r.readBy.join(', ') || '—')}</td></tr>`).join('')) : '';
    $('materializedViews').querySelectorAll('tr.clickable').forEach((tr) => {
      const pick = () => selectPlanNode(Number(tr.dataset.node));
      tr.addEventListener('click', pick);
      tr.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); pick(); } });
    });
  }

  // Select node `id` of the plan lane and show it.
  function selectPlanNode(id) {
    const cy = cys[1];
    if (!cy) return;
    const node = cy.getElementById(`n${id}`);
    if (!node || node.length === 0) return;
    cy.elements().unselect();
    node.select();
    cy.animate ? cy.animate({ center: { eles: node }, duration: 200 }) : cy.center(node);
    showNode(node.data(), 1);
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

  // Documents from outside the example list (an opened file) clear the tab.
  function showExternal(data, source) {
    ++loading;
    pressTab(null);
    try { history.replaceState(null, '', location.pathname + location.search); } catch (e) { /* ignore */ }
    try {
      showDocument(data, source, '');
    } catch (err) {
      showError(err);
    }
  }

  // Plan a `PlanningWorkload` JSON with the local server (`asap_planner`)
  // under `deploymentCapabilities` and `costModel` (null: the defaults),
  // with accuracy guarantees ignored unless `accuracyGuarantees` is
  // "enforced", and the SQL `tables` it reads (`--sql-catalog-table`
  // declarations; null for none), and show the document.
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

  $('queryPick').addEventListener('change', (e) => highlightQuery(e.target.value));
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', restyle);
  new MutationObserver(restyle).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  // Light unless the viewer picked dark (remembered per browser).
  const setTheme = (dark) => {
    document.documentElement.dataset.theme = dark ? 'dark' : 'light';
    $('themeToggle').setAttribute('aria-pressed', String(dark));
    try { localStorage.setItem('dagViewerTheme', dark ? 'dark' : 'light'); } catch (e) { /* storage blocked */ }
  };
  try { if (localStorage.getItem('dagViewerTheme') === 'dark') setTheme(true); } catch (e) { /* storage blocked */ }
  $('themeToggle').addEventListener('click', () => setTheme(document.documentElement.dataset.theme !== 'dark'));
  $('details').innerHTML = HINT;

  // Zoom in, zoom out and fit buttons over each graph.
  const graphOf = { cy0: () => cys[0], cy1: () => cys[1], cyMemo: () => window.MemoView && window.MemoView.graph() };
  document.querySelectorAll('.zoom').forEach((bar) => bar.querySelectorAll('button').forEach((b) => b.addEventListener('click', () => {
    const cy = graphOf[bar.dataset.for]();
    if (!cy) return;
    if (b.dataset.zoom === 'fit') { fitGraph(cy); return; }
    cy.zoom({ level: cy.zoom() * (b.dataset.zoom === 'in' ? 1.3 : 1 / 1.3), renderedPosition: { x: cy.width() / 2, y: cy.height() / 2 } });
  })));

  window.StageViewer = { planWorkload };

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
