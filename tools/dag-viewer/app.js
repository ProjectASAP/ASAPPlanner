// ASAPPlanner Stage Viewer: an asap-stage-pipeline/v1 document as a ranked
// Stage 3 list beside three DAG lanes (Stage 0 → 1 → 2), details below.
// Document parsing, ranking and labels live in stages.js (tested);
// node-style.js groups operator kinds. The same files run in the published
// artifact.
(function () {
  if (window.cytoscapeDagre) cytoscape.use(window.cytoscapeDagre);
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const tok = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

  let doc, ranking, queryIds, logicalById, physicalById;
  let current = { logical: null, physical: null };
  const cys = [null, null, null];

  function style() {
    return [
      { selector: 'node', style: {
          shape: 'round-rectangle', label: 'data(label)', 'text-wrap': 'wrap', 'text-valign': 'center', 'text-halign': 'center',
          'font-family': tok('--font-mono') || 'monospace', 'font-size': 10, color: tok('--fg'),
          width: 'label', height: 'label', padding: '9px', 'border-width': 1.5, 'text-max-width': 210 } },
      { selector: 'node[category = "source"]', style: { 'background-color': tok('--cat-source'), 'border-color': tok('--cat-source-line') } },
      { selector: 'node[category = "rel"]', style: { 'background-color': tok('--cat-rel'), 'border-color': tok('--cat-rel-line') } },
      { selector: 'node[category = "summary"]', style: { 'background-color': tok('--cat-summary'), 'border-color': tok('--cat-summary-line') } },
      { selector: 'node[?root]', style: { 'border-width': 3.5, 'border-color': tok('--accent') } },
      { selector: 'node:selected', style: { 'overlay-color': tok('--accent'), 'overlay-opacity': 0.14, 'overlay-padding': 5 } },
      { selector: 'edge', style: { width: 1.6, 'line-color': tok('--edge'), 'target-arrow-color': tok('--edge'), 'target-arrow-shape': 'triangle', 'curve-style': 'bezier', 'arrow-scale': 0.9 } },
      { selector: 'edge:selected', style: { 'line-color': tok('--accent'), 'target-arrow-color': tok('--accent'), width: 2.6 } },
    ];
  }

  function laneElements(dag, physical, costPerNode) {
    return stageLaneElements('lane', '', dag, { physical, costPerNode, categoryFor: nodeGroup, queryIds })
      .filter((el) => !el.data.isLane)
      .map((el) => { const data = Object.assign({}, el.data); delete data.parent; return { data, classes: el.classes }; });
  }

  function render(i, dag, physical, costPerNode) {
    if (cys[i]) cys[i].destroy();
    const cy = cytoscape({
      container: $('cy' + i), elements: laneElements(dag, physical, costPerNode), style: style(),
      layout: { name: window.cytoscapeDagre ? 'dagre' : 'breadthfirst', rankDir: 'BT', nodeSep: 18, rankSep: 34, padding: 14, directed: true },
      wheelSensitivity: 0.25, minZoom: 0.2, maxZoom: 2.5, boxSelectionEnabled: false,
    });
    cy.on('tap', 'node', (e) => showNode(e.target.data(), i));
    cy.on('tap', 'edge', (e) => showEdge(e.target.data(), i));
    cys[i] = cy;
  }

  const fmtCost = (x) => (Math.abs(x) >= 100 ? x.toFixed(1) : String(Number(x.toPrecision(3))));

  const laneName = ['Stage 0 · logical', 'Stage 1 · logical ASAP', 'Stage 2 · physical ASAP'];

  function schemaTable(schema) {
    if (!schema || !Array.isArray(schema.fields)) return '';
    const rows = schema.fields.map((f) => `<tr><td>${esc(f.name)}</td><td>${esc(compactWire(f.dtype))}</td><td>${f.nullable ? 'yes' : 'no'}</td></tr>`).join('');
    return `<div class="schema"><table><thead><tr><th>field</th><th>type</th><th>nullable</th></tr></thead><tbody>${rows}</tbody></table></div>`;
  }

  function showNode(d, lane) {
    const n = d.stageNode;
    const items = [['lane', laneName[lane]], ['operator', d.kind], ['node id', n.id]];
    if (n.payload && n.payload.kind === 'summary_agg') {
      items.push(['summary parameters', summaryFamilyText(n.payload.family)]);
      items.push(['instances', summaryInstancesText(n.payload.grouping, n.payload.reduction)]);
    }
    if (d.rootFor && d.rootFor.length) items.push(['query root of', d.rootFor.join(', ')]);
    if (d.timing) items.push(['runs at', formatTiming(d.timing, n.kept)]);
    if (n.output_state && n.output_state.primitive) items.push(['output primitive', compactWire(n.output_state.primitive)]);
    if (d.nodeCost) items.push(['Stage 3 cost', `${Number(d.nodeCost.cost.toFixed(4))} · ${d.nodeCost.detail || ''}`]);
    items.push(['coverage', n.coverage ? compactWire(n.coverage) : 'none (not a summary state)']);
    items.push(['guarantee', n.guarantee ? compactWire(n.guarantee) : 'none']);
    $('details').innerHTML = `<h3>${esc(d.kind)} <span class="eyebrow">node ${esc(n.id)}</span></h3>
      <dl class="kv">${items.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('')}</dl>
      <div><div class="eyebrow" style="margin-bottom:4px">output schema</div>${schemaTable(n.output_schema)}</div>
      <details><summary>Full operator payload (JSON)</summary><pre>${esc(JSON.stringify(n.payload, null, 2))}</pre></details>`;
  }

  function showEdge(d, lane) {
    const e = d.stageEdge;
    const items = [['lane', laneName[lane]], ['from → to', `node ${e.producer} → node ${e.consumer}`], ['role', compactWire(e.role)], ['grouping', compactWire(e.grouping)]];
    if (e.data_state) items.push(['data state', compactWire(e.data_state)]);
    if (e.window) items.push(['window', compactWire(e.window)]);
    $('details').innerHTML = `<h3>Edge <span class="eyebrow">${esc(e.producer)} → ${esc(e.consumer)}</span></h3>
      <dl class="kv">${items.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('')}</dl>
      <div><div class="eyebrow" style="margin-bottom:4px">schema on this edge</div>${schemaTable(e.intermediate_schema)}</div>`;
  }

  function statusChip(row) {
    if (row.status === 'selected') return '<span class="chip selected">✓ selected</span>';
    if (row.status === 'rejected_invalid') return '<span class="chip invalid">✗ invalid</span>';
    if (row.status === 'no_selection') return '';
    return '<span class="chip costlier">valid · costlier</span>';
  }

  function renderRanking() {
    $('rank').innerHTML = ranking.map((r) => `<li><button type="button" data-id="${esc(r.id)}" aria-pressed="${r.id === current.physical}">
        <span class="pid">${esc(r.id)}</span><span class="plabel">${esc(r.label)}</span>
        <span class="ptotal">${r.total === null ? '—' : fmtCost(r.total)}</span>
        <span class="pstatus">${statusChip(r)}${r.reason ? `<span>${esc(r.reason)}</span>` : ''}</span></button></li>`).join('');
    $('rank').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => selectPhysical(b.dataset.id)));
  }

  function selectLogical(id) {
    current.logical = id;
    $('pick1').value = id;
    const c = logicalById.get(id);
    render(1, c.dag, false, null);
    const physicalFor = doc.stage2_physical_asap.candidates.filter((p) => p.from_logical === id).map((p) => p.id);
    $('lane1-meta').innerHTML = `${esc(c.label)} · ${c.dag.nodes.length} nodes · physical plan ${physicalFor.map((p) => `<b>${esc(p)}</b>`).join(', ') || 'none'}`;
  }

  function selectPhysical(id) {
    current.physical = id;
    $('pick2').value = id;
    const c = physicalById.get(id);
    const cost = doc.stage3_selection ? doc.stage3_selection.costs[id] : null;
    render(2, c.dag, true, cost ? cost.per_node : null);
    const row = ranking.find((r) => r.id === id);
    $('lane2-meta').innerHTML = `${esc(c.label)} · from <b>${esc(c.from_logical)}</b> · ` +
      (cost ? `total <b>${fmtCost(cost.total)}</b> · rank ${row.rank} of ${ranking.filter((r) => r.total !== null).length}` : doc.stage3_selection ? 'not priced (invalid)' : 'no Stage 3 result') + ` · ${statusChip(row)}`;
    if (c.from_logical !== current.logical) selectLogical(c.from_logical);
    renderRanking();
  }

  function restyle() { cys.forEach((cy) => cy && cy.style(style())); }

  $('pick1').addEventListener('change', (e) => selectLogical(e.target.value));
  $('pick2').addEventListener('change', (e) => selectPhysical(e.target.value));
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', restyle);
  new MutationObserver(restyle).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

  const HINT = '<p class="hint">Select a node to see its operator, output schema, coverage and Stage 3 cost; select an edge to see the schema and data state it carries.</p>';
  $('details').innerHTML = HINT;

  // Show one stage document; `source` names it, `story` explains it.
  function showDocument(data, source, story) {
    data = normalizeStagePipeline(data);
    const errors = validateStagePipeline(data);
    if (errors.length) throw new Error(errors.slice(0, 3).join('; '));
    doc = data;
    current = { logical: null, physical: null };
    $('details').innerHTML = HINT;
    $('docName').textContent = source || '';
    $('story').textContent = story || '';
    $('story').hidden = !story;
    queryIds = doc.workload.queries.map((q) => q.id.toUpperCase());
    logicalById = new Map(doc.stage1_logical_asap.candidates.map((c) => [c.id, c]));
    physicalById = new Map(doc.stage2_physical_asap.candidates.map((c) => [c.id, c]));
    ranking = rankPhysicalCandidates(doc);

    $('queries').innerHTML = doc.workload.queries.map((q) => {
      const r = q.requirements || {};
      const acc = r.accuracy === 'exact' ? 'exact' : r.accuracy ? `ε=${r.accuracy.epsilon}${r.accuracy.delta !== undefined ? `, δ=${r.accuracy.delta}` : ''}` : '';
      const req = [acc && `accuracy ${acc}`, r.latency_ms && `latency ≤ ${r.latency_ms} ms`, r.repeat_interval_ms && `every ${r.repeat_interval_ms / 1000} s`].filter(Boolean).join(' · ');
      return `<div class="query"><span class="qid">${esc(q.id)} · ${esc(q.language)}</span><code>${esc(q.text)}</code>${req ? `<span class="req">${esc(req)}</span>` : ''}</div>`;
    }).join('');
    const deployment = deploymentRows(doc.deployment);
    $('deployment').innerHTML = deployment.map(([label, text]) => `<dt>${esc(label)}</dt><dd>${esc(text)}</dd>`).join('');
    $('deployment-panel').hidden = deployment.length === 0;
    const invalid = ranking.filter((r) => r.status === 'rejected_invalid').length;
    const hasStage3 = !!doc.stage3_selection;
    $('counts').innerHTML = `Candidates per stage: <b>1</b> logical DAG → <b>${logicalById.size}</b> logical ASAP → <b>${physicalById.size}</b> physical` +
      (hasStage3 ? ` → <b>1</b> selected (${invalid} invalid, ${physicalById.size - invalid - 1} valid but costlier)` : ' · no Stage 3 result');
    const shownOf = shownOfText(doc);
    $('shown-of').textContent = shownOf;
    $('shown-of').hidden = !shownOf;
    const sel = hasStage3 ? doc.stage3_selection.costs[doc.stage3_selection.selected] : null;
    $('cost-unit').textContent = sel ? sel.unit.replace(/_/g, ' ') : '';

    $('pick1').innerHTML = doc.stage1_logical_asap.candidates.map((c) => `<option value="${esc(c.id)}">${esc(c.id)} · ${esc(c.label)}</option>`).join('');
    $('pick2').innerHTML = ranking.map((r) => `<option value="${esc(r.id)}">${esc(r.id)} · ${esc(r.label)}</option>`).join('');

    render(0, doc.stage0_logical.dag, false, null);
    $('lane0-meta').innerHTML = `${doc.stage0_logical.dag.nodes.length} nodes · roots for ${queryIds.map((q) => `<b>${esc(q)}</b>`).join(', ')}`;
    const first = hasStage3 ? doc.stage3_selection.selected : ranking.length ? ranking[0].id : null;
    if (first) selectPhysical(first);
    else {
      [1, 2].forEach((i) => { if (cys[i]) { cys[i].destroy(); cys[i] = null; } });
      $('rank').innerHTML = '';
      $('lane1-meta').textContent = $('lane2-meta').textContent = 'no candidates';
    }
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

  // Documents from outside the example list (a file, the editor) clear the tab.
  function showExternal(data, source) {
    ++loading;
    pressTab(null);
    try { history.replaceState(null, '', location.pathname + location.search); } catch (e) { /* ignore */ }
    try { showDocument(data, source, ''); } catch (err) { showError(err); }
  }

  function readFile(file) {
    file.text().then((text) => showExternal(JSON.parse(text), file.name)).catch(showError);
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
  $('pick2').addEventListener('change', (e) => selectPhysical(e.target.value));
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', restyle);
  new MutationObserver(restyle).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

  window.StageViewer = { showDocument: showExternal };

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
