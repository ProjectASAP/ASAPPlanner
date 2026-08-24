// Shared viewer logic for tools/dag-viewer, used two ways:
//   1. index.html loads this via <script src="viewer.js"> for the normal
//      drag-and-drop page.
//   2. render.py inlines this file's text verbatim into a standalone HTML
//      file it generates from a dag_export JSON — see render.py's docstring
//      and the "Render a standalone page from Python" section of README.md.
// Both consumers share one copy of this logic; render.py never forks it.
cytoscape.use(window.cytoscapeDagre);

// ── State ────────────────────────────────────────────────────────────────
// queries: [{ name, graph: { nodes, root } }], flattened across every loaded
// file (a later file whose query name collides with an earlier one is kept
// distinct by suffixing the file index).
let queries = [];
let activeIndex = -1;
let cy = null;
let highlightOn = true;
let zoom = 1;
// mode: 'single' (default) shows one query's DAG via the tabs above, exactly
// as before. 'compare' lays every selected query out in its own lane, side
// by side, with dashed links between nodes that share a structural hash.
// 'union' merges selected queries into one graph, collapsing every node
// whose hash is shared by >= 2 of them into a single node with converging
// edges. See the "Compare and Union mode" section of README.md.
let mode = 'single';
// Indices into `queries` currently selected to participate in compare/union.
let participants = new Set();

const dropzone = document.getElementById('dropzone');
const fileInput = document.getElementById('fileInput');
const clearBtn = document.getElementById('clearBtn');
const highlightToggle = document.getElementById('highlightToggle');
const tabsEl = document.getElementById('tabs');
const cyOuterEl = document.getElementById('cyOuter');
const cyEl = document.getElementById('cy');
const modeHintEl = document.getElementById('modeHint');
const emptyEl = document.getElementById('empty');
const sidepanel = document.getElementById('sidepanel');
const detailSection = document.getElementById('detailSection');
const viewTitleEl = document.getElementById('viewTitle');
const zoomSlider = document.getElementById('zoomSlider');
const zoomLabel = document.getElementById('zoomLabel');
const proxyNote = document.getElementById('proxyNote');

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
  mode = 'single';
  participants = new Set();
  setModeButtons();
  render();
});
highlightToggle.addEventListener('change', () => {
  highlightOn = highlightToggle.checked;
  applyHighlighting();
});

document.querySelectorAll('#modeToggle .btn').forEach((btn) => {
  btn.addEventListener('click', () => {
    mode = btn.dataset.mode;
    setModeButtons();
    // First switch into compare/union with nothing chosen yet: default to
    // every currently-loaded query rather than an empty set of lanes.
    if (mode !== 'single' && participants.size === 0) {
      queries.forEach((_, i) => participants.add(i));
    }
    render();
  });
});

function setModeButtons() {
  document.querySelectorAll('#modeToggle .btn').forEach((b) => b.classList.toggle('active', b.dataset.mode === mode));
  proxyNote.style.display = mode === 'single' ? 'none' : 'inline';
}

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
          queries.push({ name, graph: q.graph, source: q.source });
        });
      } catch (err) {
        alert(`Failed to parse ${file.name}: ${err.message}`);
      }
      pending -= 1;
      if (pending === 0) {
        if (activeIndex === -1 && queries.length > 0) activeIndex = 0;
        render();
      }
    };
    reader.readAsText(file);
  });
  fileInput.value = '';
}

// ── Shared-subtree hashes across a set of queries ───────────────────────
// Map<hash, Set<queryName>> — a node's hash is "shared" once it shows up
// under >= 2 distinct query names (this is a client-side structural-equality
// proxy for real CSE output; see crates/types/src/dag_export.rs and the
// "Shared-subtree highlighting is a proxy" section of README.md). Defaults
// to every loaded query (single-view highlighting); Compare/Union instead
// pass just their selected participants, so a node never shows as "shared"
// against a query that isn't even part of that view.
function computeHashOwners(subsetQueries) {
  const list = subsetQueries || queries;
  const owners = new Map();
  for (const q of list) {
    const seenInThisQuery = new Set();
    for (const node of q.graph.nodes) {
      if (seenInThisQuery.has(node.hash)) continue;
      seenInThisQuery.add(node.hash);
      if (!owners.has(node.hash)) owners.set(node.hash, new Set());
      owners.get(node.hash).add(q.name);
    }
  }
  return owners;
}

function getParticipants() {
  return Array.from(participants)
    .filter((i) => i >= 0 && i < queries.length)
    .sort((a, b) => a - b);
}

function render() {
  if (queries.length === 0) {
    tabsEl.innerHTML = '';
    emptyEl.style.display = 'flex';
    cyOuterEl.style.display = 'none';
    sidepanel.style.display = 'none';
    if (cy) { cy.destroy(); cy = null; }
    return;
  }
  emptyEl.style.display = 'none';
  cyOuterEl.style.display = 'block';
  sidepanel.style.display = 'block';

  renderTabs();

  if (mode === 'single') {
    if (activeIndex === -1) activeIndex = 0;
    renderGraph(queries[activeIndex]);
  } else if (mode === 'compare') {
    renderCompare(getParticipants());
  } else {
    renderUnion(getParticipants());
  }
  renderLegend();
}

function renderTabs() {
  tabsEl.innerHTML = '';
  if (mode === 'single') {
    queries.forEach((q, i) => {
      const tab = document.createElement('div');
      tab.className = 'tab' + (i === activeIndex ? ' active' : '');
      tab.textContent = q.name;
      tab.addEventListener('click', () => { activeIndex = i; render(); });
      tabsEl.appendChild(tab);
    });
    return;
  }
  // Compare/Union: checkboxes choose which loaded queries participate.
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
      'background-image': categoryIconDataUri(name),
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
        'text-margin-y': 9,
        'font-size': 10,
        'width': 'label',
        'height': 'label',
        'padding': '16px',
        'border-width': 1.5,
        'background-fit': 'contain',
        'background-position-x': '50%',
        'background-position-y': '17%',
        'background-width': '30%',
        'background-height': '30%',
        'background-clip': 'none',
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
      selector: 'node.shared',
      style: {
        'underlay-color': ringColor,
        'underlay-opacity': 0.28,
        'underlay-padding': 6,
        'underlay-shape': 'round-rectangle',
      },
    },
    {
      // Union mode's collapsed nodes: a persistent double border marks the
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
      // Compare mode's per-query lane container (a compound parent node).
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
        'transition-property': 'opacity',
        'transition-duration': '260ms',
        'transition-timing-function': 'ease-in-out',
      },
    },
    {
      // Compare mode's cross-lane "same hash" connector: dashed, no
      // arrowhead (it isn't a data-flow edge), added after layout so it
      // never influences dagre's ranking.
      selector: 'edge.sharedLink',
      style: {
        'line-style': 'dashed',
        'line-color': ringColor,
        'target-arrow-shape': 'none',
        'source-arrow-shape': 'none',
        'curve-style': 'unbundled-bezier',
        'control-point-distances': [24],
        'control-point-weights': [0.5],
        'opacity': 0.55,
        'width': 1.5,
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

// Root badges + notes badges + click/tap wiring — identical across
// single/compare/union.
function finalizeGraphInteractions() {
  // Root nodes get a second, smaller badge icon layered in the corner —
  // QueryExpr has no dedicated terminal "output" kind the way the reference
  // repo's BGP `out_*` steps do, so the root is marked structurally instead.
  cy.nodes('[?root]').forEach((n) => {
    n.addClass('root').style({
      'background-image': [categoryIconDataUri(n.data('category')), rootBadgeIconDataUri()],
      'background-position-x': ['46%', '86%'],
      'background-position-y': ['17%', '15%'],
      'background-width': ['26%', '22%'],
      'background-height': ['26%', '22%'],
      'background-clip': ['none', 'none'],
    });
  });

  // Nodes carrying a non-empty `notes` array (issue #257: asap-aware-mapping
  // explained a replacement at this node, matched by structural hash — see
  // README.md) get a third badge in the opposite corner from the root badge,
  // so a node can be both at once without the two colliding.
  cy.nodes().forEach((n) => {
    if (n.data('isLane')) return;
    const node = n.data('node');
    if (!node || !node.notes || !node.notes.length) return;
    const isRoot = n.hasClass('root');
    n.addClass('hasNotes').style({
      'background-image': [
        categoryIconDataUri(n.data('category')),
        ...(isRoot ? [rootBadgeIconDataUri()] : []),
        noteBadgeIconDataUri(node.notes),
      ],
      'background-position-x': ['46%', ...(isRoot ? ['86%'] : []), '86%'],
      'background-position-y': ['17%', ...(isRoot ? ['15%'] : []), '83%'],
      'background-width': ['26%', ...(isRoot ? ['22%'] : []), '22%'],
      'background-height': ['26%', ...(isRoot ? ['22%'] : []), '22%'],
      'background-clip': isRoot ? ['none', 'none', 'none'] : ['none', 'none'],
    });
  });

  cy.on('tap', 'node', (evt) => {
    const n = evt.target;
    if (n.data('isLane')) return;
    if (n.data('isUnion')) {
      showUnionDetail(n.data());
      return;
    }
    const query = queries.find((q) => q.name === n.data('queryName'));
    showDetail(n.data('node'), query, mode === 'compare' ? getParticipants().map((i) => queries[i]) : undefined);
  });
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

// ── Single-query view (default, unchanged behavior) ──────────────────────

function renderGraph(query) {
  hideModeHint();
  viewTitleEl.textContent = query.name;
  renderSourcePanel([query]);

  const elements = [];
  for (const node of query.graph.nodes) {
    elements.push({
      data: {
        id: String(node.id),
        label: `${node.kind}\n${node.label}`,
        node,
        category: categoryOf(node.kind),
        root: node.id === query.graph.root,
        queryName: query.name,
      },
    });
  }
  for (const node of query.graph.nodes) {
    for (const childId of node.children) {
      elements.push({
        data: {
          // Arrow points from input to consumer (data-flow direction), the
          // reverse of the tree's parent->child structure.
          id: `e-${node.id}-${childId}`,
          source: String(childId),
          target: String(node.id),
        },
      });
    }
  }

  buildCy(elements);
  finalizeGraphInteractions();
  applyHighlighting();
  clearDetail();
  zoom = 1;
  applyZoom();
}

// ── Compare mode: selected queries side by side in lanes ─────────────────
// Each selected query gets its own compound "lane" node (a dashed box
// titled with the query name). Lanes share no structural edges with each
// other, so dagre lays them out left to right on their own. Nodes whose
// hash is shared by >= 2 selected queries get a dashed link edge added
// *after* the layout call returns (dagre layout is synchronous here), so
// those cross-lane links are purely visual and never pull dagre's ranking
// out of per-query lanes.
function renderCompare(chosen) {
  viewTitleEl.textContent = chosen.length ? `Compare: ${chosen.map((i) => queries[i].name).join(', ')}` : 'Compare';
  renderSourcePanel(chosen.map((i) => queries[i]));

  if (chosen.length < 2) {
    showModeHint('Select two or more loaded queries above to compare them side by side.');
    return;
  }
  hideModeHint();

  const elements = [];
  chosen.forEach((qIdx) => {
    const q = queries[qIdx];
    const laneId = `lane-${qIdx}`;
    elements.push({
      data: { id: laneId, label: q.name, isLane: true },
      classes: 'laneParent',
      selectable: false,
      grabbable: false,
    });
    for (const node of q.graph.nodes) {
      elements.push({
        data: {
          id: `q${qIdx}-${node.id}`,
          parent: laneId,
          label: `${node.kind}\n${node.label}`,
          node,
          category: categoryOf(node.kind),
          root: node.id === q.graph.root,
          queryName: q.name,
        },
      });
    }
    for (const node of q.graph.nodes) {
      for (const childId of node.children) {
        elements.push({
          data: {
            // Arrow points from input to consumer (data-flow direction), the
            // reverse of the tree's parent->child structure.
            id: `e-q${qIdx}-${node.id}-${childId}`,
            source: `q${qIdx}-${childId}`,
            target: `q${qIdx}-${node.id}`,
          },
        });
      }
    }
  });

  buildCy(elements);
  cy.add(buildSharedLinkEdges(chosen));
  finalizeGraphInteractions();
  applyHighlighting();
  clearDetail();
  fitAndSyncZoom();
}

// Dashed, non-directional link edges connecting one representative node per
// lane for every hash shared by >= 2 of the chosen queries. Chains each
// occurrence to the nearest earlier lane that also has it (not a full
// mesh), which is enough to make "this shape recurs" visually obvious
// without drawing an edge for every pair.
function buildSharedLinkEdges(chosen) {
  const owners = computeHashOwners(chosen.map((i) => queries[i]));
  const links = [];
  for (const [hash, ownerNames] of owners) {
    if (ownerNames.size < 2) continue;
    let prevKey = null;
    chosen.forEach((qIdx) => {
      const q = queries[qIdx];
      if (!ownerNames.has(q.name)) return;
      const representative = q.graph.nodes.find((n) => n.hash === hash);
      if (!representative) return;
      const key = `q${qIdx}-${representative.id}`;
      if (prevKey) {
        links.push({
          data: { id: `link-${hash}-${prevKey}-${key}`, source: prevKey, target: key },
          classes: 'sharedLink',
          selectable: false,
        });
      }
      prevKey = key;
    });
  }
  return links;
}

// ── Union mode: merge selected queries into one graph ─────────────────────
// A node whose hash is shared by >= 2 selected queries collapses into a
// single graph node (id `h-<hash>`); every other node keeps a per-query id
// (`q<i>-<nodeId>`). Edges are deduped after the merge, so multiple parents
// — from different queries, or repeats within one query — that point at the
// same shared node converge onto it instead of each drawing a separate
// copy. That's the "real branching DAG" case this mode has to handle:
// a merged node can end up with several parents at once, and plain dagre
// already lays out multi-parent DAGs natively, so no special-cased layout
// is needed beyond building this merged element set.
function renderUnion(chosen) {
  viewTitleEl.textContent = chosen.length ? `Union: ${chosen.map((i) => queries[i].name).join(', ')}` : 'Union';
  renderSourcePanel(chosen.map((i) => queries[i]));

  if (chosen.length < 2) {
    showModeHint('Select two or more loaded queries above to merge them into one union graph.');
    return;
  }
  hideModeHint();

  const owners = computeHashOwners(chosen.map((i) => queries[i]));
  const isSharedHash = (hash) => {
    const ownerNames = owners.get(hash);
    return !!ownerNames && ownerNames.size > 1;
  };
  const keyFor = (qIdx, node) => (isSharedHash(node.hash) ? `h-${node.hash}` : `q${qIdx}-${node.id}`);

  const nodeEntries = new Map(); // key -> accumulator
  const edgeKeys = new Set();
  const elements = [];

  chosen.forEach((qIdx) => {
    const q = queries[qIdx];
    for (const node of q.graph.nodes) {
      const key = keyFor(qIdx, node);
      let entry = nodeEntries.get(key);
      if (!entry) {
        entry = {
          node,
          category: categoryOf(node.kind),
          isMerged: isSharedHash(node.hash),
          sourceQueries: new Set(),
          rootFor: new Set(),
        };
        nodeEntries.set(key, entry);
      }
      entry.sourceQueries.add(q.name);
      if (node.id === q.graph.root) entry.rootFor.add(q.name);
    }
  });

  chosen.forEach((qIdx) => {
    const q = queries[qIdx];
    const byId = new Map(q.graph.nodes.map((n) => [n.id, n]));
    for (const node of q.graph.nodes) {
      const parentKey = keyFor(qIdx, node);
      for (const childId of node.children) {
        const childNode = byId.get(childId);
        const childKey = keyFor(qIdx, childNode);
        const edgeKey = `${parentKey}__${childKey}`;
        if (edgeKeys.has(edgeKey)) continue;
        edgeKeys.add(edgeKey);
        // Arrow points from input to consumer (data-flow direction), the
        // reverse of the tree's parent->child structure.
        elements.push({ data: { id: `e-${edgeKey}`, source: childKey, target: parentKey } });
      }
    }
  });

  for (const [key, entry] of nodeEntries) {
    elements.push({
      data: {
        id: key,
        label: `${entry.node.kind}\n${entry.node.label}`,
        node: entry.node,
        category: entry.category,
        isUnion: true,
        isMerged: entry.isMerged,
        sourceQueries: Array.from(entry.sourceQueries),
        rootFor: Array.from(entry.rootFor),
        root: entry.rootFor.size > 0,
      },
      classes: entry.isMerged ? 'unionShared' : '',
    });
  }

  buildCy(elements);
  finalizeGraphInteractions();
  applyHighlighting();
  clearDetail();
  fitAndSyncZoom();
}

// ── Shared rendering bits ─────────────────────────────────────────────────

function renderSourcePanel(qs) {
  const sourceBox = document.getElementById('sourceBox');
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

function applyHighlighting() {
  if (!cy) return;
  const owners = mode === 'single' ? computeHashOwners() : computeHashOwners(getParticipants().map((i) => queries[i]));
  cy.nodes().forEach((n) => {
    if (n.data('isLane') || n.data('isUnion')) return;
    const node = n.data('node');
    if (!node) return;
    const sharedWith = owners.get(node.hash);
    const isShared = highlightOn && sharedWith && sharedWith.size > 1;
    n.toggleClass('shared', isShared);
  });
}

function clearDetail() {
  detailSection.innerHTML = '<h2>Selected node</h2><div class="placeholder">Click a node to inspect it.</div>';
}

function showDetail(node, query, scopeQueries) {
  const owners = computeHashOwners(scopeQueries);
  const sharedWith = Array.from(owners.get(node.hash) || []).filter((n) => n !== query.name);
  const category = categoryOf(node.kind);
  const catColors = categoryColors(category);
  const catLabel = (CATEGORIES[category] || {}).label || category;

  const rootHtml = node.id === query.graph.root
    ? `<div class="rootNote">This is the query's root (final output).</div>`
    : '';
  const sharedHtml = sharedWith.length
    ? `<div class="shared-note">Structurally identical to a node also present in: ${sharedWith
        .map(escapeHtml)
        .join(', ')}</div>`
    : '';

  detailSection.innerHTML = `
    <h2>Selected node</h2>
    <span class="chip" style="color:${catColors.border}; background:${catColors.bg}">${escapeHtml(catLabel)} · ${escapeHtml(node.kind)}</span>
    <div style="font-weight:650; margin:0.3rem 0 0.4rem">${escapeHtml(node.label)}</div>
    ${rootHtml}
    ${sharedHtml}
    ${notesHtml(node)}
    <pre>${escapeHtml(JSON.stringify(node.detail, null, 2))}</pre>
  `;
}

// Renders a DagNode's `notes` (issue #257: asap-aware-mapping's explanation
// of why a replacement exists here, matched onto this node by structural
// hash — see README.md), one block per note, color-coded to match its badge.
// Empty string if there are none, so callers can always splice this in.
function notesHtml(node) {
  if (!node.notes || !node.notes.length) return '';
  const items = node.notes
    .map((note) => {
      const c = noteKindColor(note.kind);
      return `<div class="noteItem" style="border-left-color:${c}">
        <span class="noteKind" style="color:${c}">${escapeHtml(note.kind)}</span>
        <div class="noteReason">${escapeHtml(note.reason)}</div>
      </div>`;
    })
    .join('');
  return `<div class="notesBlock"><h3>Why a replacement exists here</h3>${items}</div>`;
}

// Union mode's variant of showDetail: `data` is a merged cytoscape node's
// data object (see renderUnion), not a raw DagNode + query pair, since a
// merged node can be "present in" several queries and "root of" several
// (or zero) of them at once.
function showUnionDetail(data) {
  const node = data.node;
  const category = categoryOf(node.kind);
  const catColors = categoryColors(category);
  const catLabel = (CATEGORIES[category] || {}).label || category;

  const rootHtml = data.rootFor && data.rootFor.length
    ? `<div class="rootNote">Root (final output) of: ${data.rootFor.map(escapeHtml).join(', ')}</div>`
    : '';
  const sharedHtml = data.isMerged
    ? `<div class="shared-note">Merged: same structural hash in ${data.sourceQueries.length} selected queries — ${data.sourceQueries
        .map(escapeHtml)
        .join(', ')}</div>`
    : `<div class="shared-note" style="color:var(--muted)">Query-specific to ${escapeHtml(data.sourceQueries[0])}</div>`;

  detailSection.innerHTML = `
    <h2>Selected node</h2>
    <span class="chip" style="color:${catColors.border}; background:${catColors.bg}">${escapeHtml(catLabel)} · ${escapeHtml(node.kind)}</span>
    <div style="font-weight:650; margin:0.3rem 0 0.4rem">${escapeHtml(node.label)}</div>
    ${rootHtml}
    ${sharedHtml}
    ${notesHtml(node)}
    <pre>${escapeHtml(JSON.stringify(node.detail, null, 2))}</pre>
  `;
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
    <span><span class="swatchLabel">Shared subtree</span><span class="swatchDesc">Structurally identical to a node in another loaded query (hash-based proxy, not real CSE)</span></span></div>`);
  rows.push(`<div class="leg"><span class="swatch ring" style="border-color:${rootColor}"></span>
    <span><span class="swatchLabel">Query root</span><span class="swatchDesc">${escapeHtml(ROOT_BADGE.description)}</span></span></div>`);
  for (const [kind, label] of Object.entries(NOTE_BADGE_LABEL)) {
    const c = noteKindColor(kind);
    rows.push(`<div class="leg"><span class="swatch" style="background:${c}; border-color:${c}"></span>
      <span><span class="swatchLabel">${escapeHtml(label)}</span><span class="swatchDesc">Bottom-right badge — click the node for why (issue #257)</span></span></div>`);
  }
  if (mode === 'compare') {
    rows.push(`<div class="leg"><span class="swatch ring" style="border-color:${ringColor}; border-style:dashed"></span>
      <span><span class="swatchLabel">Shared-subtree link</span><span class="swatchDesc">Dashed line connects matching-hash nodes across lanes</span></span></div>`);
  }
  if (mode === 'union') {
    rows.push(`<div class="leg"><span class="swatch" style="background:transparent; border:3px double ${ringColor}"></span>
      <span><span class="swatchLabel">Merged node</span><span class="swatchDesc">Collapsed: this hash appears in 2+ selected queries</span></span></div>`);
  }
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
  incoming.forEach((q) => queries.push({ name: q.name, graph: q.graph, source: q.source }));
  if (activeIndex === -1 && queries.length > 0) activeIndex = 0;
}

// render.py's standalone output bakes the WorkloadGraph JSON directly into
// the page as a <script type="application/json"> tag instead of relying on
// fetch('dag.json') — a `file://` page can't fetch a sibling file in most
// browsers (blocked as cross-origin), which is exactly the "no browser
// dev-server available" case that tool exists for. `window.__DAG_RENDER__`
// is an optional config object the same generated page may also set, e.g.
// `{ mode: 'union' }`, to open straight into Compare/Union instead of
// Single — see render.py's --mode flag.
const embeddedEl = document.getElementById('embedded-workload');
if (embeddedEl) {
  try {
    loadWorkload(JSON.parse(embeddedEl.textContent));
  } catch (err) {
    console.error('tools/dag-viewer: failed to parse embedded workload data', err);
  }
  const requestedMode = window.__DAG_RENDER__ && window.__DAG_RENDER__.mode;
  if (requestedMode && requestedMode !== 'single') {
    mode = requestedMode;
    queries.forEach((_, i) => participants.add(i));
    setModeButtons();
  }
  render();
} else {
  // Plain index.html: no embedded data. If a dag.json sits next to this page
  // (e.g. so a remote/tunnelled session has something to look at without a
  // local file to drag in), load it automatically. Silently does nothing if
  // there's no such file — drag/drop and the file picker still work either
  // way. Prefer a real, freshly-generated dag.json (gitignored scratch
  // output of generate-sample.sh or a manual dag_export run), falling back
  // to the committed dag.example.json so the page still shows something
  // with zero setup.
  fetch('dag.json')
    .then((r) => (r.ok ? r : fetch('dag.example.json')))
    .then((r) => (r.ok ? r.json() : null))
    .then(loadWorkload)
    .catch(() => {})
    .finally(render);
}
