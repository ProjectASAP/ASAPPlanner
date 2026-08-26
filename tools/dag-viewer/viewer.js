// Shared viewer logic for tools/dag-viewer, used two ways:
//   1. index.html loads this via <script src="viewer.js"> for the normal
//      drag-and-drop page.
//   2. render.py inlines this file's text verbatim into a standalone HTML
//      file it generates from a dag_export JSON — see render.py's docstring
//      and the "Render a standalone page from Python" section of README.md.
// Both consumers share one copy of this logic; render.py never forks it.
cytoscape.use(window.cytoscapeDagre);

// ── State ────────────────────────────────────────────────────────────────
// queries: [{ name, graph: { nodes, root }, replacements?, post_graph? }],
// flattened across every loaded file (a later file whose query name
// collides with an earlier one is kept distinct by suffixing the file
// index). `replacements` is the optional --post-asap array of
// ReplacementSite entries for that query (each with its own `before`/`after`
// subtree) — defaulted to `[]` when the loaded JSON omits the field, so
// callers never need an extra existence check. `post_graph` is the optional
// --post-asap whole-query merged post-ASAP graph (same flattened
// `{nodes, root}` shape as `graph`, but nodes may be post-ASAP-only kinds
// like "SummaryAgg" mixed in, and any such node has no `hash` — there's no
// corresponding QueryExpr to hash) — left `undefined` when absent (omitted
// whenever --post-asap wasn't set, or this query had zero replacements),
// unlike `replacements` which always defaults to an array.
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
  queries = [];
  activeIndex = -1;
  participants = new Set();
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
  const files = Array.from(fileList || []);
  let pending = files.length;
  if (pending === 0) return;
  files.forEach((file, fileIdx) => {
    const reader = new FileReader();
    reader.onload = () => {
      try {
        const parsed = JSON.parse(reader.result);
        const incoming = parsed.queries || [];
        const existingNames = new Set(queries.map((q) => q.name));
        incoming.forEach((q) => {
          let name = q.name;
          if (existingNames.has(name)) name = `${q.name} (${file.name})`;
          existingNames.add(name);
          queries.push({ name, graph: q.graph, source: q.source, replacements: q.replacements || [], post_graph: q.post_graph });
        });
      } catch (err) {
        alert(`Failed to parse ${file.name}: ${err.message}`);
      }
      pending -= 1;
      if (pending === 0) {
        if (activeIndex === -1 && queries.length > 0) activeIndex = 0;
        if (participants.size === 0 && activeIndex >= 0) participants.add(activeIndex);
        render();
      }
    };
    reader.readAsText(file);
  });
  fileInput.value = '';
}

function getParticipants() {
  return Array.from(participants)
    .filter((i) => i >= 0 && i < queries.length)
    .sort((a, b) => a - b);
}

function render() {
  if (queries.length === 0) {
    tabsEl.innerHTML = '';
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

  renderTabs();

  renderPrePostAsap();
  renderLegend();
}

function renderTabs() {
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
      // KeepPreAsap (post-ASAP lane only) is post-ASAP-only
      // as a *kind*, but represents literally unchanged pre-ASAP content —
      // override the 'summary' category's color/icon with the same neutral
      // panel/muted/dashed treatment the rest of the chrome uses for "nothing
      // to see here", so a glance at the After lane separates "the planner
      // did something" (solid, colored) from "left alone" (dashed, muted).
      // See node-style.js's CATEGORIES.summary comment for the category-level
      // color choice this overrides.
      selector: 'node[kind = "KeepPreAsap"]',
      style: {
        'background-color': panelColor,
        'border-color': borderColor,
        'border-style': 'dashed',
      },
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
// makes the graph read as "assembling" instead of flickering to a new
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
function finalizeGraphInteractions() {
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
  const missing = selected.filter((q) => !q.post_graph);
  if (missing.length > 0) {
    viewTitleEl.textContent = selected.length === 1 ? `Pre/Post-ASAP: ${selected[0].name}` : `Pre/Post-ASAP workload: ${selected.length} queries`;
    const action = document.getElementById('plannerRun')
      ? 'Open Query planner and click “Plan selected workload”, or re-export with dag_export --post-asap.'
      : 'Re-export with dag_export --post-asap.';
    showModeHint(`No post-ASAP graph for: ${missing.map((q) => q.name).join(', ')}. ${action}`);
    return;
  }
  hideModeHint();
  viewTitleEl.textContent = selected.length === 1
    ? `Pre/Post-ASAP: ${selected[0].name}`
    : `Pre/Post-ASAP workload union: ${selected.length} queries`;

  const elements = selected.length === 1
    ? [
        ...laneElements('pre-asap', `${selected[0].name} · pre-ASAP`, selected[0].graph, selected[0], 'pre'),
        ...laneElements('post-asap', `${selected[0].name} · post-ASAP`, selected[0].post_graph, selected[0], 'post'),
      ]
    : [
        ...unionStageLaneElements('pre', chosen),
        ...unionStageLaneElements('post', chosen),
      ];

  buildCy(elements);
  finalizeGraphInteractions();
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
  // Workload merging is an exporter decision. `workload_node_id` is the
  // explicit JSON mapping; do not reconstruct identity from node content.
  const laneId = `${stage}-asap-union`;
  const graphFor = (query) => (stage === 'pre' ? query.graph : query.post_graph);
  const owners = new Map();

  chosen.forEach((qIdx) => {
    const query = queries[qIdx];
    const graph = graphFor(query);
    graph.nodes.forEach((node) => {
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
    { data: { id: laneId, label: `${stage === 'pre' ? 'pre-ASAP' : 'post-ASAP'} · workload union`, isLane: true }, classes: 'laneParent', selectable: false, grabbable: false },
  ];

  chosen.forEach((qIdx) => {
    const query = queries[qIdx];
    const graph = graphFor(query);
    const byId = new Map(graph.nodes.map((node) => [node.id, node]));
    graph.nodes.forEach((node) => {
      const key = keyFor(qIdx, node);
      let entry = entries.get(key);
      if (!entry) {
        entry = { node, sourceQueries: new Set(), rootFor: new Set(), decisions: new Map() };
        entries.set(key, entry);
      }
      entry.sourceQueries.add(query.name);
      for (const decision of translationsForNode(query, node, stage)) {
        entry.decisions.set(decision.id, decision);
      }
      if (node.id === graph.root) entry.rootFor.add(query.name);
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
      translations: Array.from(entry.decisions.values()),
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
// nodes/edges). `nodes` is either a plain pre-ASAP
// DagNode list (the `before` subtree, or an `after.kind === "Rewrite"`
// graph) or a SummaryDagNode list (an `after.kind === "Summary"` graph) —
// both shapes carry id/kind/label/detail/children, which is all a lane
// needs; SummaryDagNode's missing `hash`/`notes` fields are simply never
// read by this function or by showPrePostDetail below.
function laneElements(laneId, laneLabel, graph, query, stage) {
  const nodes = graph.nodes;
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const elements = [
    { data: { id: laneId, label: laneLabel, isLane: true }, classes: 'laneParent', selectable: false, grabbable: false },
  ];
  for (const node of nodes) {
    elements.push({
      data: {
        id: `${laneId}-${node.id}`,
        parent: laneId,
        label: node.label,
        node,
        // Flat (not nested under `node`) so buildCyStyle's
        // `node[kind = "KeepPreAsap"]` selector can actually match it —
        // cytoscape selectors can't reach into a data field that's itself an
        // object.
        kind: node.kind,
        category: categoryOf(node.kind),
        root: node.id === graph.root,
        isPrePost: true,
        laneId,
        stage,
        queryName: query.name,
        translations: translationsForNode(query, node, stage),
      },
    });
  }
  for (const node of nodes) {
    for (const childId of node.children) {
      elements.push({
        data: {
          // Arrow points from input to consumer (data-flow direction), the
          // reverse of the tree's parent->child structure.
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
  const fields = Array.isArray(schema.columns) ? schema.columns : schema.fields;
  if (!Array.isArray(fields) || fields.length === 0) return 'empty schema';
  const rows = fields.map((field, index) => {
    const name = field && field.name !== undefined ? field.name : '?';
    const dtype = field && field.dtype !== undefined
      ? (typeof field.dtype === 'string' ? field.dtype : JSON.stringify(field.dtype))
      : '?';
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
    default: return `${node.kind} applies the IR operation shown below; its declared output schema is exported by the planner.`;
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
  const strategies = new Set();
  selected.forEach((query) => (query.post_graph?.nodes || []).forEach((node) => {
    if (node.decision && node.decision.rank === 0) strategies.add(node.decision.strategy);
  }));
  const scope = selected.length === 1 ? 'Single query' : `Batch workload · ${selected.length} queries`;
  const strategyText = strategies.size ? `Winning strategies: ${Array.from(strategies).join(', ')}` : 'No selected replacements';
  scopePickerEl.innerHTML = `<div class="scopeGroup"><div class="scopeGroupLabel">View scope</div><div class="scopeRow active"><span>${escapeHtml(scope)}</span><span class="scopeMeta">${escapeHtml(strategyText)}</span></div></div>`;
}

function translationsForNode(query, node, stage) {
  // The pre lane is deliberately the original pre-ASAP IR only. Strategy
  // metadata belongs to nodes in the post lane.
  if (stage === 'pre') return [];
  const replacements = (query.replacements || []).filter((replacement) => replacement.rank === 0);
  if (!node.decision) return [];
  const replacement = replacements.find((entry) => entry.decision_id === node.decision.id);
  return [{
    ...node.decision,
    target_pre_id: replacement ? replacement.target_pre_id : undefined,
    output_kind: replacement ? replacement.after?.kind : undefined,
  }];
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

  const decisions = data.translations || [];
  let translationHtml = '';
  if (decisions.length > 0) {
    const cards = decisions.map((entry) => `
      <div class="translationCard">
        <div class="translationStrategy">${escapeHtml(entry.strategy)}</div>
        <div class="translationMeta">rank ${entry.rank} · estimated cost ${escapeHtml(entry.cost)}</div>
        <div class="translationMeta">${entry.role === 'replacement_root' ? 'This node replaces the pre-ASAP target.' : 'This node is generated or carried inside the replacement region.'}</div>
        <div class="translationReason">${escapeHtml(entry.rationale || 'No rationale recorded.')}</div>
        <div class="translationTarget">${entry.target_pre_id === undefined ? `decision #${entry.id}` : `pre-ASAP target node #${entry.target_pre_id}`} · output ${escapeHtml(entry.output_kind || node.kind)}</div>
      </div>`).join('');
    translationHtml = `<div class="translationBlock"><h3>Why this post-ASAP translation</h3>${cards}</div>`;
  } else if (data.stage === 'post') {
    translationHtml = `<div class="translationBlock unchanged"><h3>Translation</h3><div>No replacement targets this node; it is unchanged support structure in the post-ASAP DAG.</div></div>`;
  }

  detailSection.innerHTML = `
    <h2>Selected node</h2>
    <span class="chip" style="color:${catColors.border}; background:${catColors.bg}">${escapeHtml(chipLabel)}</span>
    <div style="font-weight:650; margin:0.3rem 0 0.4rem">${escapeHtml(node.label)}</div>
    ${rootHtml}
    ${translationHtml}
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
      sourceBox.textContent = 'No source text in this export.';
      sourceBox.classList.add('placeholder');
    }
    return;
  }
  const blocks = qs.map((q) => `— ${q.name} —\n${q.source || '(no source text in this export)'}`);
  sourceBox.textContent = blocks.join('\n\n');
  sourceBox.classList.remove('placeholder');
}

function renderTableSchemas(qs) {
  const box = document.getElementById('tableSchemaBox');
  const schemas = new Map();
  qs.forEach((query) => {
    (query.graph?.nodes || []).filter((node) => node.kind === 'Scan').forEach((node) => {
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
  const ownersFor = (graphOf) => {
    const owners = new Map();
    selected.forEach((query) => {
      const seen = new Set();
      const graph = graphOf(query);
      (graph ? graph.nodes : []).forEach((node) => {
        const id = node.workload_node_id;
        if (id === undefined || seen.has(id)) return;
        seen.add(id);
        if (!owners.has(id)) owners.set(id, new Set());
        owners.get(id).add(query.name);
      });
    });
    return owners;
  };
  const preOwners = ownersFor((query) => query.graph);
  const postOwners = ownersFor((query) => query.post_graph);
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
    <span><span class="swatchLabel">Shared workload node</span><span class="swatchDesc">Explicitly identified by the exporter as shared across selected queries</span></span></div>`);
  rows.push(`<div class="leg"><span class="swatch ring" style="border-color:${rootColor}"></span>
    <span><span class="swatchLabel">Query root</span><span class="swatchDesc">${escapeHtml(ROOT_BADGE.description)}</span></span></div>`);
  const panelBg = getComputedStyle(document.documentElement).getPropertyValue('--panel2').trim() || '#f0f2f5';
  const mutedColor = getComputedStyle(document.documentElement).getPropertyValue('--muted').trim() || '#6b7280';
  rows.push(`<div class="leg"><span class="swatch" style="background:${panelBg}; border-color:${mutedColor}; border-style:dashed"></span>
    <span><span class="swatchLabel">Pass-through (KeepPreAsap)</span><span class="swatchDesc">Unchanged pre-ASAP subtree carried into the Summary graph as-is</span></span></div>`);
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

function loadWorkload(parsed) {
  const incoming = (parsed && parsed.queries) || [];
  incoming.forEach((q) => queries.push({ name: q.name, graph: q.graph, source: q.source, replacements: q.replacements || [], post_graph: q.post_graph }));
  if (activeIndex === -1 && queries.length > 0) activeIndex = 0;
  if (participants.size === 0 && activeIndex >= 0) participants.add(activeIndex);
}

// Entry point used by planner-ui.js after the local HTTP backend returns a
// freshly generated WorkloadGraph. Replace (rather than append to) the
// current data and open the complete workload pre/post view.
window.renderPlannerWorkload = function renderPlannerWorkload(parsed) {
  queries = [];
  participants = new Set();
  activeIndex = -1;
  loadWorkload(parsed);
  queries.forEach((_, index) => participants.add(index));
  render();
};

// render.py's standalone output bakes the WorkloadGraph JSON directly into
// the page as a <script type="application/json"> tag instead of relying on
// fetch('dag.json') — a `file://` page can't fetch a sibling file in most
// browsers (blocked as cross-origin), which is exactly the "no browser
// dev-server available" case that tool exists for. `window.__DAG_RENDER__`
// is an optional config object the same generated page may also set, e.g.
// The generated page always opens in Pre/Post-ASAP mode.
const embeddedEl = document.getElementById('embedded-workload');
if (embeddedEl) {
  try {
    loadWorkload(JSON.parse(embeddedEl.textContent));
  } catch (err) {
    console.error('tools/dag-viewer: failed to parse embedded workload data', err);
  }
  if (queries.length > 0 && participants.size === 0) participants.add(0);
  render();
} else {
  // Plain index.html starts with the committed, post-ASAP-generated example.
  // Do not silently prefer a leftover scratch dag.json: it may predate
  // post_graph and make Pre/Post-ASAP appear broken. Users can load scratch
  // exports explicitly with the picker or run them through Query planner.
  fetch('/api/example', { cache: 'no-store' })
    .then((r) => (r.ok ? r.json() : fetch('dag.example.json', { cache: 'no-store' }).then((fallback) => fallback.json())))
    .then(loadWorkload)
    .catch(() => {})
    .finally(render);
}
