// Viewer logic for tools/dag-viewer, used two ways:
//   1. index.html loads this via <script src="viewer.js">.
//   2. render.py inlines this file verbatim into a standalone HTML page that
//      embeds one stage_pipeline document.
// The viewer reads only `asap-stage-pipeline/v1` documents (stage-workload.js).
cytoscape.use(window.cytoscapeDagre);

// ── State ────────────────────────────────────────────────────────────────
// stageDocument: the loaded asap-stage-pipeline/v1 document.
// queries: stageWorkload(stageDocument).queries — [{ name, source, dag,
// post_dag? }], `dag` the query's part of Stage 0 and `post_dag` its part of
// the Stage 3 selected plan (absent when Stage 3 selected none).
let stageDocument = null;
let queries = [];
let activeIndex = -1;
let cy = null;
let highlightOn = true;
let zoom = 1;
// The viewer has one Pre/Post-ASAP mode. One selected query renders its own
// two DAGs; multiple selected queries union each stage into one workload DAG.
let participants = new Set();
const dropzone = document.getElementById('dropzone');
const fileInput = document.getElementById('fileInput');
const clearBtn = document.getElementById('clearBtn');
const highlightToggle = document.getElementById('highlightToggle');
const tabsEl = document.getElementById('tabs');
const selectAllTab = document.getElementById('selectAllTab');
const selectAllToggle = document.getElementById('selectAllToggle');
const scopePickerEl = document.getElementById('scopePicker');
const cyOuterEl = document.getElementById('cyOuter');
const cyEl = document.getElementById('cy');
const modeHintEl = document.getElementById('modeHint');
const emptyEl = document.getElementById('empty');
const sidepanel = document.getElementById('sidepanel');
const sideResizeHandle = document.getElementById('sideResizeHandle');
const detailSection = document.getElementById('detailSection');
const viewTitleEl = document.getElementById('viewTitle');
const zoomSlider = document.getElementById('zoomSlider');
const zoomLabel = document.getElementById('zoomLabel');

dropzone.addEventListener('click', () => fileInput.click());
dropzone.addEventListener('dragover', (e) => { e.preventDefault(); dropzone.classList.add('dragover'); });
dropzone.addEventListener('dragleave', () => dropzone.classList.remove('dragover'));
dropzone.addEventListener('drop', (e) => {
  e.preventDefault();
  dropzone.classList.remove('dragover');
  loadFiles(e.dataTransfer.files);
});
fileInput.addEventListener('change', (e) => loadFiles(e.target.files));
clearBtn.addEventListener('click', () => {
  stageDocument = null;
  queries = [];
  activeIndex = -1;
  participants = new Set();
  render();
});
// Selection only. `Clear all` above drops the loaded workload itself, which on
// a render.py page cannot be undone — its embedded workload is parsed once, at
// load — so the two stay separate controls rather than one overloaded button.
//
// A tri-state checkbox rather than two buttons: "some selected" is a real
// third state with fifty queries, and `indeterminate` is what shows it. The
// browser resolves a click on an indeterminate box to `checked = true`, so
// "partially selected" clicks through to select-all, which is the useful
// direction.
selectAllToggle.addEventListener('change', () => {
  participants = selectAllToggle.checked
    ? new Set(queries.map((_, index) => index))
    : new Set();
  render();
});
highlightToggle.addEventListener('change', () => {
  highlightOn = highlightToggle.checked;
  applyHighlighting();
});
let resizingSidepanel = false;
sideResizeHandle.addEventListener('pointerdown', (event) => {
  resizingSidepanel = true;
  sideResizeHandle.classList.add('dragging');
  sideResizeHandle.setPointerCapture(event.pointerId);
});
sideResizeHandle.addEventListener('pointermove', (event) => {
  if (!resizingSidepanel) return;
  const width = Math.max(260, Math.min(window.innerWidth * 0.7, window.innerWidth - event.clientX));
  sidepanel.style.width = `${width}px`;
  if (cy) cy.resize();
});
sideResizeHandle.addEventListener('pointerup', (event) => {
  resizingSidepanel = false;
  sideResizeHandle.classList.remove('dragging');
  sideResizeHandle.releasePointerCapture(event.pointerId);
});

function loadFiles(fileList) {
  const file = Array.from(fileList || [])[0];
  if (!file) return;
  const reader = new FileReader();
  reader.onload = () => {
    try {
      const parsed = JSON.parse(reader.result);
      if (!isStagePipelineDocument(parsed)) throw new Error(`not an ${STAGE_PIPELINE_FORMAT} document; write one with stage_pipeline`);
      showDocument(parsed);
    } catch (err) {
      alert(`Failed to load ${file.name}: ${err.message}`);
    }
  };
  reader.readAsText(file);
  fileInput.value = '';
}

function getParticipants() {
  return Array.from(participants)
    .filter((i) => i >= 0 && i < queries.length)
    .sort((a, b) => a - b);
}

function render() {
  renderTabs();
  if (queries.length === 0) {
    scopePickerEl.classList.remove('visible');
    emptyEl.style.display = 'flex';
    cyOuterEl.style.display = 'none';
    sidepanel.style.display = 'none';
    sideResizeHandle.style.display = 'none';
    if (cy) { cy.destroy(); cy = null; }
    return;
  }
  emptyEl.style.display = 'none';
  cyOuterEl.style.display = 'block';
  sidepanel.style.display = 'block';
  sideResizeHandle.style.display = 'block';

  renderPrePostAsap();
  renderLegend();
}

// The three states the bulk-selection checkbox has to distinguish: none,
// some, all. `indeterminate` is the only one of the three that a plain
// checkbox cannot show on its own, and it is the common case with a
// fifty-query workload. Pure, so it can be tested without a DOM.
function bulkSelectionState(queryCount, selectedCount) {
  const all = queryCount > 0 && selectedCount === queryCount;
  return {
    // Nothing loaded: the control would claim a selection that has no
    // queries to apply to.
    hidden: queryCount === 0,
    checked: all,
    indeterminate: selectedCount > 0 && !all,
  };
}

function renderTabs() {
  // Derived from `participants` on every render rather than tracked by the
  // change handler: every other path that writes the selection (file load,
  // `Clear all`, renderPlannerWorkload, an individual chip) also lands in
  // `render`, and a control that only followed its own clicks would drift
  // out of step with them.
  const bulk = bulkSelectionState(queries.length, participants.size);
  selectAllTab.hidden = bulk.hidden;
  selectAllToggle.checked = bulk.checked;
  selectAllToggle.indeterminate = bulk.indeterminate;
  selectAllTab.classList.toggle('active', bulk.checked);

  tabsEl.innerHTML = '';
  // Checkboxes choose the single query or workload to union.
  queries.forEach((q, i) => {
    const chip = document.createElement('label');
    chip.className = 'tab checkTab' + (participants.has(i) ? ' active' : '');
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = participants.has(i);
    cb.addEventListener('change', () => {
      if (cb.checked) participants.add(i); else participants.delete(i);
      render();
    });
    chip.appendChild(cb);
    chip.appendChild(document.createTextNode(' ' + q.name));
    tabsEl.appendChild(chip);
  });
}

// ── Shared cytoscape setup, used by single/compare/union ─────────────────

function buildCyStyle() {
  const textColor = getComputedStyle(document.documentElement).getPropertyValue('--fg').trim() || '#1b1f24';
  const edgeColor = getComputedStyle(document.documentElement).getPropertyValue('--muted').trim() || '#9aa1ab';
  const ringColor = getComputedStyle(document.documentElement).getPropertyValue('--accent').trim() || '#2563eb';
  const panelColor = getComputedStyle(document.documentElement).getPropertyValue('--panel2').trim() || '#f0f2f5';
  const borderColor = getComputedStyle(document.documentElement).getPropertyValue('--border').trim() || '#d9dce1';

  const categoryStyles = Object.keys(CATEGORIES).map((name) => ({
    selector: `node[category = "${name}"]`,
    style: {
      'border-color': categoryColors(name).border,
      'background-color': categoryColors(name).bg,
    },
  }));

  const style = [
    {
      selector: 'node',
      style: {
        'shape': 'round-rectangle',
        'corner-radius': 10,
        'label': 'data(label)',
        'color': textColor,
        'text-wrap': 'wrap',
        'text-valign': 'center',
        'text-halign': 'center',
        'font-size': 10,
        'width': 'label',
        'height': 'label',
        'padding': '12px',
        'border-width': 1.5,
        // Lets class/style changes below (the '.shared' ring, '.unionShared'
        // double border, ':selected' outline, and buildCy's fade-in) animate
        // instead of snapping — cytoscape treats these like CSS transitions,
        // separate from (and layered on top of) the layout animation that
        // moves nodes to their laid-out position.
        'transition-property': 'underlay-opacity, border-width, border-color, opacity',
        'transition-duration': '260ms',
        'transition-timing-function': 'ease-in-out',
      },
    },
    {
      selector: 'node[category = "data"]',
      style: { 'corner-radius': 999, 'border-style': 'dashed' },
    },
    ...categoryStyles,
    {
      selector: 'node[category = "unknown"]',
      style: { 'border-style': 'dashed', 'border-width': 3 },
    },
    {
      selector: 'node.root',
      style: { 'border-width': 2.5 },
    },
    {
      selector: 'node.shared',
      style: {
        'underlay-color': ringColor,
        'underlay-opacity': 0.28,
        'underlay-padding': 6,
        'underlay-shape': 'round-rectangle',
      },
    },
    {
      // Workload-union collapsed nodes: a persistent double border marks the
      // merge itself (unlike '.shared', this isn't gated by the highlight
      // toggle — collapsing *is* how union mode represents sharing).
      selector: 'node.unionShared',
      style: {
        'border-width': 3,
        'border-style': 'double',
      },
    },
    {
      selector: 'node:selected',
      style: {
        'border-width': 3,
        'border-color': ringColor,
      },
    },
    {
      // Pre/Post-ASAP lane container (a compound parent node).
      selector: 'node.laneParent',
      style: {
        'shape': 'round-rectangle',
        'corner-radius': 14,
        'background-opacity': 0.35,
        'background-color': panelColor,
        'border-width': 1,
        'border-style': 'dashed',
        'border-color': borderColor,
        'label': 'data(label)',
        'text-valign': 'top',
        'text-halign': 'center',
        'text-margin-y': -10,
        'font-size': 12,
        'font-weight': 700,
        'color': textColor,
        'padding': '30px',
      },
    },
    {
      selector: 'edge',
      style: {
        'width': 1.5,
        'line-color': edgeColor,
        'target-arrow-color': edgeColor,
        'target-arrow-shape': 'triangle',
        'curve-style': 'bezier',
        'label': '',
        'font-size': 9,
        'color': textColor,
        'text-background-color': panelColor,
        'text-background-opacity': 0.92,
        'text-background-padding': 3,
        'text-rotation': 'autorotate',
        'text-wrap': 'wrap',
        'text-max-width': 260,
        'transition-property': 'opacity',
        'transition-duration': '260ms',
        'transition-timing-function': 'ease-in-out',
      },
    },
  ];

  return style;
}

// Default layout animates dagre's computed positions in rather than snapping
// to them — every mode switch, checkbox toggle, and file load rebuilds `cy`
// from scratch (see the `cy.destroy()` below), so this animation is what
// makes the DAG read as "assembling" instead of flickering to a new
// static image each time. `elements` also start at opacity 0 and fade in
// (paired with the 'node'/'edge' transition-property set in buildCyStyle)
// so a freshly-added element doesn't just pop in mid-layout-animation.
const LAYOUT_ANIMATION = { animate: true, animationDuration: 550, animationEasing: 'ease-out-cubic' };

function buildCy(elements, layout) {
  if (cy) { cy.destroy(); cy = null; }
  cy = cytoscape({
    container: cyEl,
    elements,
    style: buildCyStyle(),
    layout: layout ? { ...LAYOUT_ANIMATION, ...layout } : { name: 'dagre', rankDir: 'TB', nodeSep: 30, rankSep: 60, ...LAYOUT_ANIMATION },
  });
  cy.elements().style({ opacity: 0 });
  // Double rAF: the first frame just lets the browser paint the opacity:0
  // state so there's something to transition *from*; flipping to opacity:1
  // in the same frame that set opacity:0 would collapse to a no-op.
  requestAnimationFrame(() => requestAnimationFrame(() => {
    if (cy) cy.elements().style({ opacity: 1 });
  }));
  return cy;
}

// Root borders and click/tap wiring for the Pre/Post-ASAP lanes.
function finalizeDAGInteractions() {
  // Use borders rather than pictograms so IR text owns the whole node box.
  cy.nodes('[?root]').addClass('root');

  cy.on('tap', 'node', (evt) => {
    const n = evt.target;
    if (n.data('isLane')) return;
    showPrePostDetail(n.data());
  });
  cy.on('tap', 'edge', (evt) => showEdgeDetail(evt.target));
  cy.on('tap', (evt) => { if (evt.target === cy) clearDetail(); });
}

function fitAndSyncZoom() {
  if (!cy) return;
  cy.fit(undefined, 30);
  zoom = cy.zoom();
  zoomSlider.value = Math.round(zoom * 100);
  zoomLabel.textContent = Math.round(zoom * 100) + '%';
}

function showModeHint(text) {
  if (cy) { cy.destroy(); cy = null; }
  modeHintEl.textContent = text;
  modeHintEl.style.display = 'flex';
  cyEl.style.display = 'none';
}

function hideModeHint() {
  modeHintEl.style.display = 'none';
  cyEl.style.display = 'block';
}

// ── Pre/Post-ASAP: one query or two workload-union DAGs ──────────────────
function renderPrePostAsap() {
  const chosen = getParticipants();
  const selected = chosen.map((i) => queries[i]);
  renderScopeSummary(selected);
  renderSourcePanel(selected);

  if (selected.length === 0) {
    viewTitleEl.textContent = 'Pre/Post-ASAP';
    showModeHint('Select one query for a complete pre/post DAG, or several queries for a batch workload view.');
    return;
  }
  // Without a selected plan (planning failed), Stage 0 is still shown.
  const planned = selected.every((q) => q.post_dag);
  hideModeHint();
  viewTitleEl.textContent = (selected.length === 1
    ? `Pre/Post-ASAP: ${selected[0].name}`
    : `Pre/Post-ASAP workload union: ${selected.length} queries`)
    + (planned ? '' : ' · no plan selected; see the candidate table');

  const elements = selected.length === 1
    ? [
        ...laneElements('pre-asap', `${selected[0].name} · Stage 0 query-language frontends`, selected[0].dag, selected[0], 'pre'),
        ...(planned ? laneElements('post-asap', `${selected[0].name} · Stage 3 selected plan`, selected[0].post_dag, selected[0], 'post') : []),
      ]
    : [
        ...unionStageLaneElements('pre', chosen),
        ...(planned ? unionStageLaneElements('post', chosen) : []),
      ];

  buildCy(elements);
  finalizeDAGInteractions();
  applyHighlighting();
  const initial = cy.nodes().filter((node) =>
    !node.data('isLane') && node.data('stage') === 'pre' && node.data('root')
  ).first();
  if (initial && initial.length) {
    initial.select();
    showPrePostDetail(initial.data());
  } else {
    clearDetail();
  }
  fitAndSyncZoom();
}

function unionStageLaneElements(stage, chosen) {
  // Workload merging is the planner's decision. `workload_node_id` is the
  // explicit JSON mapping; do not reconstruct identity from node content.
  const laneId = `${stage}-asap-union`;
  const dagFor = (query) => (stage === 'pre' ? query.dag : query.post_dag);
  const owners = new Map();

  chosen.forEach((qIdx) => {
    const query = queries[qIdx];
    const dag = dagFor(query);
    dag.nodes.forEach((node) => {
      if (node.workload_node_id === undefined) return;
      if (!owners.has(node.workload_node_id)) owners.set(node.workload_node_id, new Set());
      owners.get(node.workload_node_id).add(query.name);
    });
  });

  const sharedIds = new Map();
  let nextSharedId = 0;
  owners.forEach((queryNames, workloadNodeId) => {
    if (queryNames.size > 1) sharedIds.set(workloadNodeId, `${laneId}-shared-${nextSharedId++}`);
  });
  const keyFor = (qIdx, node) => sharedIds.get(node.workload_node_id) || `${laneId}-q${qIdx}-${node.id}`;
  const entries = new Map();
  const edges = new Map();
  const elements = [
    // `pannable: true`: a drag starting on the lane's own background pans the
    // viewport. `grabbable: false` alone made it a dead zone — cytoscape
    // begins a pan only when the element under the pointer is `pannable()`,
    // which defaults to false for nodes, and the lane covers most of the
    // canvas once the DAG is zoomed past the viewport. Still not grabbable:
    // cytoscape overrides `grabbable` for a pannable node.
    { data: { id: laneId, label: `${stage === 'pre' ? 'Stage 0 query-language frontends' : 'Stage 3 selected plan'} · workload union`, isLane: true }, classes: 'laneParent', selectable: false, grabbable: false, pannable: true },
  ];

  chosen.forEach((qIdx) => {
    const query = queries[qIdx];
    const dag = dagFor(query);
    const byId = new Map(dag.nodes.map((node) => [node.id, node]));
    dag.nodes.forEach((node) => {
      const key = keyFor(qIdx, node);
      let entry = entries.get(key);
      if (!entry) {
        entry = { node, sourceQueries: new Set(), rootFor: new Set() };
        entries.set(key, entry);
      }
      entry.sourceQueries.add(query.name);
      if (node.id === dag.root) entry.rootFor.add(query.name);
      node.children.forEach((childId) => {
        const childKey = keyFor(qIdx, byId.get(childId));
        const child = byId.get(childId);
        edges.set(`${childKey}\u0000${key}`, formatSchema(child.schema));
      });
    });
  });

  entries.forEach((entry, key) => elements.push({
    data: {
      id: key,
      parent: laneId,
      label: entry.node.label,
      node: entry.node,
      kind: entry.node.kind,
      category: categoryOf(entry.node.kind),
      root: entry.rootFor.size > 0,
      rootFor: Array.from(entry.rootFor),
      sourceQueries: Array.from(entry.sourceQueries),
      isPrePost: true,
      stage,
      queryName: Array.from(entry.sourceQueries)[0],
    },
    classes: entry.sourceQueries.size > 1 ? 'unionShared' : '',
  }));
  let edgeIndex = 0;
  edges.forEach((schemaLabel, edge) => {
    const [source, target] = edge.split('\u0000');
    elements.push({ data: { id: `${laneId}-edge-${edgeIndex++}`, source, target, schemaLabel } });
  });
  return elements;
}

// Builds one Pre/Post-ASAP lane (a dashed compound parent plus its
// nodes/edges) from a stageWorkload DAG: nodes carry
// id/kind/label/schema/detail/children.
function laneElements(laneId, laneLabel, dag, query, stage) {
  const nodes = dag.nodes;
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const elements = [
    // Pannable for the same reason as the union lane above.
    { data: { id: laneId, label: laneLabel, isLane: true }, classes: 'laneParent', selectable: false, grabbable: false, pannable: true },
  ];
  for (const node of nodes) {
    elements.push({
      data: {
        id: `${laneId}-${node.id}`,
        parent: laneId,
        label: node.label,
        node,
        // Cytoscape selectors read flat data fields.
        kind: node.kind,
        category: categoryOf(node.kind),
        root: node.id === dag.root,
        isPrePost: true,
        laneId,
        stage,
        queryName: query.name,
      },
    });
  }
  for (const node of nodes) {
    for (const childId of node.children) {
      elements.push({
        data: {
          // Arrow points from input to consumer (data-flow direction), the
          // reverse of the DAG's parent->child structure.
          id: `e-${laneId}-${node.id}-${childId}`,
          source: `${laneId}-${childId}`,
          target: `${laneId}-${node.id}`,
          schemaLabel: formatSchema(byId.get(childId).schema),
        },
      });
    }
  }
  return elements;
}

function formatSchema(schema) {
  if (!schema || typeof schema !== 'object') return 'schema unavailable';
  const fields = Array.isArray(schema.fields) ? schema.fields : schema.columns;
  if (!Array.isArray(fields) || fields.length === 0) return 'empty schema';
  const rows = fields.map((field, index) => {
    const name = field && field.name !== undefined ? field.name : '?';
    // A plain value is tagged `{"Plain": <dtype>}`.
    const raw = field && field.dtype !== undefined ? field.dtype : '?';
    const plain = raw && typeof raw === 'object' && 'Plain' in raw ? raw.Plain : raw;
    const dtype = typeof plain === 'string' ? plain : JSON.stringify(plain);
    return {
      name: String(name),
      dtype: String(dtype).toUpperCase(),
      nullable: field && field.nullable ? 'NULL' : 'NOT NULL',
      timeIndex: schema.time_index === index,
    };
  });
  const nameWidth = Math.max(...rows.map((row) => row.name.length));
  const typeWidth = Math.max(...rows.map((row) => row.dtype.length));
  return rows.map((row) =>
    `${row.name.padEnd(nameWidth)}  ${row.dtype.padEnd(typeWidth)}  ${row.nullable}${row.timeIndex ? '  · TIME INDEX' : ''}`
  ).join('\n');
}

function schemaDerivation(target) {
  const node = target.data('node');
  if (!node) return 'The target operation metadata is unavailable.';
  const detail = node.detail || {};
  switch (node.kind) {
    case 'Filter': return 'Filter evaluates its predicate and preserves the input columns and types.';
    case 'Sort': return 'Sort changes row order using its sort keys and preserves the input schema.';
    case 'Limit': return `Limit keeps ${detail.n ?? 'the requested number of'} ordered rows and preserves the input schema.`;
    case 'Project': return 'Project evaluates its selected expressions; aliases and expression result types form the output schema.';
    case 'Aggregate': return 'Aggregate emits its reduction/group-by keys followed by the result column for each aggregate measure.';
    case 'SummaryAgg': return 'SummaryAgg replaces the aggregate state with the selected exact or approximate summary representation.';
    case 'SummaryEstimate': return 'SummaryEstimate reads the selected summary state and emits the requested estimate schema.';
    case 'TimeRange': return 'TimeRange restricts the temporal input window and preserves the row schema.';
    case 'Scan': return 'Scan obtains this schema from the bound table or metric source.';
    default: return `${node.kind} applies the IR operation shown below; its declared output schema is in the stage document.`;
  }
}

function showEdgeDetail(edge) {
  const source = edge.source();
  const target = edge.target();
  const sourceNode = source.data('node') || {};
  const targetNode = target.data('node') || {};
  const edgeSchema = edge.data('schemaLabel') || formatSchema(sourceNode.schema);
  detailSection.innerHTML = `
    <h2>Selected edge</h2>
    <div class="translationBlock">
      <h3>Connection</h3>
      <div><strong>From:</strong> ${escapeHtml(sourceNode.label || source.id())}</div>
      <div><strong>To:</strong> ${escapeHtml(targetNode.label || target.id())}</div>
    </div>
    <h3 class="detailSubhead">Schema carried by this edge</h3>
    <pre>${escapeHtml(edgeSchema || '(schema unavailable)')}</pre>
    <h3 class="detailSubhead">How the schema is produced</h3>
    <div class="translationBlock">${escapeHtml(schemaDerivation(target))}</div>
    <h3 class="detailSubhead">Target operation</h3>
    <pre>${escapeHtml(JSON.stringify(targetNode.detail || {}, null, 2))}</pre>
    <h3 class="detailSubhead">Target output schema</h3>
    <pre>${escapeHtml(formatSchema(targetNode.schema) || '(schema unavailable)')}</pre>
  `;
}

function renderScopeSummary(selected) {
  scopePickerEl.classList.add('visible');
  const scope = selected.length === 1 ? 'Single query' : `Batch workload · ${selected.length} queries`;
  const rows = stageCandidateRows(stageDocument).map((row) => `
    <tr class="candidate candidate--${row.status.replace(' ', '-')}">
      <td>${escapeHtml(row.id)}</td>
      <td>${escapeHtml(row.display_name || '')}</td>
      <td>${row.total === null ? '' : `${escapeHtml(Number(row.total.toPrecision(4)))} ${escapeHtml(row.unit || '')}`}</td>
      <td>${escapeHtml(row.status)}${row.reason ? `: ${escapeHtml(row.reason)}` : ''}</td>
    </tr>`).join('');
  scopePickerEl.innerHTML = `<div class="scopeGroup"><div class="scopeGroupLabel">View scope</div><div class="scopeRow active"><span>${escapeHtml(scope)}</span></div></div>
    <div class="scopeGroup"><div class="scopeGroupLabel">${escapeHtml(stageCandidateHeader(stageDocument))}</div>
    <table class="candidateTable"><thead><tr><th>Candidate</th><th>Choices</th><th>Cost</th><th>Outcome</th></tr></thead><tbody>${rows}</tbody></table></div>`;
}

function showPrePostDetail(data) {
  const node = data.node;
  const category = categoryOf(node.kind);
  const catColors = categoryColors(category);
  const catLabel = (CATEGORIES[category] || {}).label || category;
  const stageName = data.stage === 'pre' ? 'pre-ASAP' : 'post-ASAP';
  const chipLabel = catLabel === node.kind ? node.kind : `${catLabel} · ${node.kind}`;

  const rootNames = data.rootFor && data.rootFor.length ? data.rootFor : [data.queryName];
  const rootHtml = data.root
    ? `<div class="rootNote">Root of the complete ${stageName} DAG for: ${rootNames.map(escapeHtml).join(', ')}.</div>`
    : '';

  const cost = node.detail && node.detail.cost;
  const costHtml = cost
    ? `<div class="translationBlock"><h3>Stage 3 cost</h3><div>${escapeHtml(cost.cost)} × ${escapeHtml(cost.multiplier)}${cost.detail ? ` · ${escapeHtml(cost.detail)}` : ''}</div></div>`
    : '';

  detailSection.innerHTML = `
    <h2>Selected node</h2>
    <span class="chip" style="color:${catColors.border}; background:${catColors.bg}">${escapeHtml(chipLabel)}</span>
    <div style="font-weight:650; margin:0.3rem 0 0.4rem">${escapeHtml(node.label)}</div>
    ${rootHtml}
    ${costHtml}
    <h3 class="detailSubhead">IR node content</h3>
    <pre>${escapeHtml(JSON.stringify(node.detail, null, 2))}</pre>
  `;
}

// ── Shared rendering bits ─────────────────────────────────────────────────

function renderSourcePanel(qs) {
  const sourceBox = document.getElementById('sourceBox');
  renderTableSchemas(qs);
  if (qs.length === 0) {
    sourceBox.textContent = 'Select queries above.';
    sourceBox.classList.add('placeholder');
    return;
  }
  if (qs.length === 1) {
    const q = qs[0];
    if (q.source) {
      sourceBox.textContent = q.source;
      sourceBox.classList.remove('placeholder');
    } else {
      sourceBox.textContent = 'No source text in this document.';
      sourceBox.classList.add('placeholder');
    }
    return;
  }
  const blocks = qs.map((q) => `— ${q.name} —\n${q.source || '(no source text in this document)'}`);
  sourceBox.textContent = blocks.join('\n\n');
  sourceBox.classList.remove('placeholder');
}

function renderTableSchemas(qs) {
  const box = document.getElementById('tableSchemaBox');
  const schemas = new Map();
  qs.forEach((query) => {
    (query.dag?.nodes || []).filter((node) => node.kind === 'Scan').forEach((node) => {
      const key = JSON.stringify([node.detail, node.schema]);
      if (!schemas.has(key)) schemas.set(key, { node, owners: [] });
      schemas.get(key).owners.push(query.name);
    });
  });
  const blocks = Array.from(schemas.values()).map(({ node, owners }) =>
    `${node.label}\nUsed by: ${owners.join(', ')}\n${formatSchema(node.schema) || '(schema unavailable)'}`);
  box.textContent = blocks.join('\n\n') || 'No bound Scan schema in the selected queries.';
  box.classList.toggle('placeholder', blocks.length === 0);
}

function applyHighlighting() {
  if (!cy) return;
  const selected = getParticipants().map((i) => queries[i]);
  const ownersFor = (dagOf) => {
    const owners = new Map();
    selected.forEach((query) => {
      const seen = new Set();
      const dag = dagOf(query);
      (dag ? dag.nodes : []).forEach((node) => {
        const id = node.workload_node_id;
        if (id === undefined || seen.has(id)) return;
        seen.add(id);
        if (!owners.has(id)) owners.set(id, new Set());
        owners.get(id).add(query.name);
      });
    });
    return owners;
  };
  const preOwners = ownersFor((query) => query.dag);
  const postOwners = ownersFor((query) => query.post_dag);
  cy.nodes().forEach((element) => {
    if (element.data('isLane')) return;
    const node = element.data('node');
    const owners = element.data('stage') === 'post' ? postOwners : preOwners;
    const sharedWith = node && owners.get(node.workload_node_id);
    element.toggleClass('shared', !!(highlightOn && sharedWith && sharedWith.size > 1));
  });
}

function clearDetail() {
  detailSection.innerHTML = '<h2>Selected node</h2><div class="placeholder">Click a node to inspect it.</div>';
}

function renderLegend() {
  const legendList = document.getElementById('legendList');
  const rows = Object.entries(CATEGORIES).map(([name, cat]) => {
    const c = categoryColors(name);
    return `<div class="leg"><span class="swatch" style="background:${c.bg}; border-color:${c.border}"></span>
      <span><span class="swatchLabel">${escapeHtml(cat.label)}</span><span class="swatchDesc">${escapeHtml(cat.description)}</span></span></div>`;
  });
  const ringColor = getComputedStyle(document.documentElement).getPropertyValue('--accent').trim() || '#2563eb';
  const rootColor = getComputedStyle(document.documentElement).getPropertyValue('--cat-output-border').trim() || '#b42318';
  rows.push(`<div class="leg"><span class="swatch ring" style="border-color:${ringColor}"></span>
    <span><span class="swatchLabel">Shared workload node</span><span class="swatchDesc">One stage DAG node shared by the selected queries</span></span></div>`);
  rows.push(`<div class="leg"><span class="swatch ring" style="border-color:${rootColor}"></span>
    <span><span class="swatchLabel">Query root</span><span class="swatchDesc">${escapeHtml(ROOT_BADGE.description)}</span></span></div>`);
  legendList.innerHTML = rows.join('');
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function applyZoom() {
  if (!cy) return;
  cy.zoom(zoom);
  cy.center();
  zoomSlider.value = Math.round(zoom * 100);
  zoomLabel.textContent = Math.round(zoom * 100) + '%';
}

document.getElementById('zoomSlider').addEventListener('input', (e) => { zoom = Number(e.target.value) / 100; applyZoom(); });
document.getElementById('zoomOutBtn').addEventListener('click', () => { zoom = Math.max(0.25, zoom - 0.1); applyZoom(); });
document.getElementById('zoomInBtn').addEventListener('click', () => { zoom = Math.min(2.5, zoom + 0.1); applyZoom(); });
document.getElementById('fitBtn').addEventListener('click', () => { if (cy) { cy.fit(undefined, 30); zoom = cy.zoom(); zoomSlider.value = Math.round(zoom * 100); zoomLabel.textContent = Math.round(zoom * 100) + '%'; } });
document.getElementById('resetBtn').addEventListener('click', () => { zoom = 1; applyZoom(); });

// Show `doc` (an asap-stage-pipeline/v1 document), replacing what is
// loaded, with every query selected. Used by file loads, planner-ui.js and
// the startup load below.
function showDocument(doc) {
  stageDocument = doc;
  queries = stageWorkload(doc).queries;
  activeIndex = queries.length ? 0 : -1;
  participants = new Set(queries.map((_, index) => index));
  render();
}
window.renderPlannerWorkload = showDocument;

// render.py's standalone page embeds the document as a
// <script type="application/json"> tag: a `file://` page can't fetch a
// sibling file in most browsers.
const embeddedEl = document.getElementById('embedded-workload');
if (embeddedEl) {
  try {
    showDocument(JSON.parse(embeddedEl.textContent));
  } catch (err) {
    console.error('tools/dag-viewer: failed to parse the embedded document', err);
    render();
  }
} else {
  // Plain index.html starts with the committed example document.
  fetch('/api/example', { cache: 'no-store' })
    .then((r) => (r.ok ? r.json() : fetch('dag.example.json', { cache: 'no-store' }).then((fallback) => fallback.json())))
    .then(showDocument)
    .catch(() => render());
}
