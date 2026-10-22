// Workload sets: the PlanningWorkload files listed in workloads/index.json,
// one table per set. Clicking a workload plans it through the local server
// (`stage_pipeline --workload`, default search, the set's deployment
// capabilities) and shows the stage lanes. Without the server, a workload
// with a precomputed stage document (`document`) opens that document; the
// others are marked as needing the server.
(function () {
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  let sets = [];
  let serverUp = false;
  const workloads = new Map(); // file → PlanningWorkload JSON

  function setById(id) { return sets.find((set) => set.id === id); }

  // A deployment-capabilities object as text: "raw data not retained".
  function capabilitiesText(capabilities) {
    if (!capabilities || typeof capabilities !== 'object') return 'default deployment capabilities';
    const parts = [];
    if (capabilities.raw_data_retained === false) parts.push('raw data not retained');
    if (capabilities.query_time_results_retained === true) parts.push('query-time results retained');
    if (Array.isArray(capabilities.supported_summaries)) parts.push(`${capabilities.supported_summaries.length} supported summary families`);
    return parts.join(', ') || 'default deployment capabilities';
  }

  // How a set's workloads are planned: the search, its deployment
  // capabilities, cost model and accuracy guarantee mode (ignored, as
  // stage_pipeline does by default, unless the set says "enforced").
  function plannedWith(set) {
    return `default search, ${capabilitiesText(set.deployment_capabilities)}${set.cost_model ? `, cost model ${set.cost_model}` : ''}` +
      `, accuracy guarantees ${set.accuracy_guarantees === 'enforced' ? 'enforced' : 'ignored'}`;
  }

  function renderSet(set) {
    $('setMeta').innerHTML = `${esc(set.description || '')}<br><span>Source: ${esc(set.source || 'not given')}</span><br>` +
      `<span>Planned with: ${esc(plannedWith(set))}.</span>`;
    const rows = set.workloads.map((entry, index) => {
      const workload = workloads.get(entry.file);
      if (!workload) {
        return `<tr><td>${esc(entry.id)}</td><td colspan="4"><span class="error">${esc(entry.file)} did not load</span></td></tr>`;
      }
      const queries = planningWorkloadQueries(workload);
      const labels = Array.isArray(entry.query_labels) && entry.query_labels.length === queries.length ? entry.query_labels : queries.map((_, k) => `q${k + 1}`);
      const sources = Object.keys(((workload.data_workload || {}).sources) || {});
      const read = sources.filter((name) => queriesNaming(name, queries.map((q, k) => ({ id: labels[k], text: q.text }))).length);
      const list = queries.map((q, k) => `<li><code>${esc(q.text)}</code> <span class="req">${esc(labels[k])} · ${esc(q.runs)} · accuracy ${esc(q.accuracy)}</span></li>`).join('');
      const offline = entry.document ? 'precomputed' : 'needs the server';
      const status = serverUp ? 'plan' : offline;
      return `<tr class="clickable" tabindex="0" data-index="${index}"><td>${esc(entry.id)}</td><td>${queries.length}</td>` +
        `<td class="wrap">${esc(labels.join(', '))}<details><summary>queries</summary><ul>${list}</ul></details></td>` +
        `<td class="wrap">${esc(read.join(', ') || '—')}</td><td><span class="chip${status === 'needs the server' ? ' invalid' : ''}">${esc(status)}</span></td></tr>`;
    }).join('');
    $('setTable').innerHTML = `<table><thead><tr><th>workload</th><th>queries</th><th class="wrap">query templates</th><th class="wrap">sources read</th><th>open</th></tr></thead><tbody>${rows}</tbody></table>`;
    $('setTable').querySelectorAll('tr.clickable').forEach((tr) => {
      const open = (e) => { if (e.target.closest('details')) return; openWorkload(set, set.workloads[Number(tr.dataset.index)]); };
      tr.addEventListener('click', open);
      tr.addEventListener('keydown', (e) => { if (e.key === 'Enter') open(e); });
    });
  }

  function openWorkload(set, entry) {
    const workload = workloads.get(entry.file);
    const source = `${set.id} / ${entry.id}`;
    const story = `${set.name || set.id}, ${entry.id}: planned with the ${plannedWith(set)}.`;
    if (!serverUp) {
      if (entry.document) window.StageViewer.loadDocument(entry.document, `${source} (precomputed)`, story);
      else $('setsServer').textContent = `${entry.id} needs the server: start server.py`;
      return;
    }
    $('setsServer').textContent = `planning ${entry.id}…`;
    window.StageViewer.planWorkload(workload, set.deployment_capabilities || null, source, story, set.cost_model || null, set.accuracy_guarantees || null)
      .then(() => { $('setsServer').textContent = 'planned by the local server'; })
      .catch((err) => {
        // Fall back to the precomputed document when the server cannot plan.
        $('setsServer').textContent = `the server could not plan ${entry.id}: ${err.message}`;
        if (entry.document) window.StageViewer.loadDocument(entry.document, `${source} (precomputed)`, story);
      });
  }

  function show(open) {
    $('workloadSets').hidden = !open;
    $('setsToggle').setAttribute('aria-pressed', String(open));
  }

  $('setsToggle').addEventListener('click', () => show($('workloadSets').hidden));
  $('setPick').addEventListener('change', (e) => renderSet(setById(e.target.value)));

  const json = (url) => fetch(url).then((r) => { if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`); return r.json(); });
  // The server answers /api/status; a static host does not.
  const status = fetch('/api/status').then((r) => r.ok).catch(() => false);
  json('workloads/index.json').then((index) => {
    sets = Array.isArray(index && index.sets) ? index.sets : [];
    if (!sets.length) return;
    const files = sets.flatMap((set) => set.workloads.map((entry) => entry.file));
    return Promise.all([status, ...files.map((file) => json(`workloads/${file}`).then((w) => workloads.set(file, w)).catch(() => {}))]).then(([up]) => {
      serverUp = up;
      $('setsServer').textContent = up ? 'click a workload to plan it with the local server' : 'no server: precomputed workloads only';
      $('setPick').innerHTML = sets.map((set) => `<option value="${esc(set.id)}">${esc(set.name || set.id)} (${set.workloads.length})</option>`).join('');
      $('setsToggle').hidden = false;
      renderSet(sets[0]);
      if (location.hash === '#workload-sets') show(true);
    });
  }).catch(() => { /* no workload sets next to the page */ });
})();
