const plannerPanel = document.getElementById('plannerPanel');
const plannerRows = document.getElementById('plannerRows');
const plannerStatus = document.getElementById('plannerStatus');
const schemaRows = document.getElementById('schemaRows');

function addSchemaRow(schema = {}) {
  const row = document.createElement('div');
  row.className = 'schemaRow';
  row.innerHTML = `
    <input class="schemaInclude" type="checkbox" checked title="Include this table in the SQL catalog" />
    <input class="schemaName" value="${escapeHtml(schema.name || `table_${schemaRows.children.length + 1}`)}" aria-label="Table name" />
    <textarea class="schemaColumns" rows="2" aria-label="Columns">${escapeHtml(schema.columns || 'id:int64!, value:float64')}</textarea>
    <input class="schemaTimeIndex" value="${escapeHtml(schema.timeIndex ?? '')}" placeholder="time column" aria-label="Time-index column" />
    <button class="btn schemaRemove" type="button">Remove</button>`;
  row.querySelector('.schemaRemove').addEventListener('click', () => {
    row.remove();
    refreshSchemaSelectors();
  });
  row.querySelectorAll('input, textarea').forEach((input) => input.addEventListener('input', refreshSchemaSelectors));
  schemaRows.appendChild(row);
  refreshSchemaSelectors();
}

function enabledSchemaNames() {
  return Array.from(schemaRows.querySelectorAll('.schemaRow'))
    .filter((row) => row.querySelector('.schemaInclude').checked)
    .map((row) => row.querySelector('.schemaName').value.trim())
    .filter(Boolean);
}

function refreshSchemaSelectors() {
  const names = enabledSchemaNames();
  plannerRows.querySelectorAll('.plannerRow').forEach((row) => {
    const container = row.querySelector('.plannerSchemas');
    const previous = new Set(Array.from(container.querySelectorAll('input:checked')).map((input) => input.value));
    const promql = row.querySelector('.plannerLanguage').value === 'promql';
    container.innerHTML = promql
      ? '<span>Metric schema (inferred)</span>'
      : names.map((name) => `<label><input type="checkbox" value="${escapeHtml(name)}" ${previous.has(name) ? 'checked' : ''}/>${escapeHtml(name)}</label>`).join('');
  });
}

function parseSchemaRow(row) {
  const columns = row.querySelector('.schemaColumns').value.split(',').map((raw) => raw.trim()).filter(Boolean).map((raw) => {
    const match = raw.match(/^([^:]+):([a-zA-Z0-9]+)(!)?$/);
    if (!match) throw new Error(`Invalid column ${raw}; use name:type or name:type!`);
    return { name: match[1].trim(), type: match[2], nullable: !match[3] };
  });
  const timeName = row.querySelector('.schemaTimeIndex').value.trim();
  const timeIndex = timeName ? columns.findIndex((column) => column.name === timeName) : -1;
  if (timeName && timeIndex < 0) throw new Error(`Time column ${timeName} is not in the column list`);
  return {
    name: row.querySelector('.schemaName').value.trim(),
    columns,
    ...(timeIndex >= 0 ? { time_index: timeIndex } : {}),
  };
}

function addPlannerRow(query = {}) {
  const row = document.createElement('div');
  row.className = 'plannerRow';
  row.innerHTML = `
    <input class="plannerInclude" type="checkbox" checked title="Include in workload" />
    <input class="plannerName" value="${escapeHtml(query.name || `q${plannerRows.children.length + 1}`)}" aria-label="Query name" />
    <select class="plannerLanguage" aria-label="Language"><option value="sql">SQL</option><option value="promql">PromQL</option></select>
    <div class="plannerSchemas" aria-label="Input schemas"></div>
    <textarea class="plannerText" aria-label="Query text" rows="2">${escapeHtml(query.text || 'SELECT service, COUNT(*) FROM metrics GROUP BY service')}</textarea>
    <button class="btn plannerRemove" type="button">Remove</button>`;
  row.querySelector('.plannerLanguage').value = query.language || 'sql';
  row.querySelector('.plannerLanguage').addEventListener('change', refreshSchemaSelectors);
  row.querySelector('.plannerRemove').addEventListener('click', () => row.remove());
  plannerRows.appendChild(row);
  refreshSchemaSelectors();
  (query.schemas || (query.schema ? [query.schema] : [])).forEach((name) => {
    const checkbox = Array.from(row.querySelectorAll('.plannerSchemas input')).find((input) => input.value === name);
    if (checkbox) checkbox.checked = true;
  });
}

document.getElementById('plannerToggle').addEventListener('click', () => plannerPanel.classList.toggle('visible'));
document.getElementById('plannerAdd').addEventListener('click', () => addPlannerRow({ text: '' }));
document.getElementById('schemaAdd').addEventListener('click', () => addSchemaRow());
document.getElementById('plannerRun').addEventListener('click', async () => {
  const rows = Array.from(plannerRows.querySelectorAll('.plannerRow')).filter((row) => row.querySelector('.plannerInclude').checked);
  const queries = rows.map((row) => ({
    name: row.querySelector('.plannerName').value,
    language: row.querySelector('.plannerLanguage').value,
    schemas: Array.from(row.querySelectorAll('.plannerSchemas input:checked')).map((input) => input.value),
    text: row.querySelector('.plannerText').value,
  }));
  plannerStatus.textContent = 'Running pre-ASAP lowering and ASAP-aware mapping…';
  document.getElementById('plannerRun').disabled = true;
  try {
    const response = await fetch('/api/plan', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        queries,
        schemas: Array.from(schemaRows.querySelectorAll('.schemaRow'))
          .filter((row) => row.querySelector('.schemaInclude').checked)
          .map(parseSchemaRow),
        epsilon: Number(document.getElementById('plannerEpsilon').value),
      }),
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || `HTTP ${response.status}`);
    window.renderPlannerWorkload(result);
    plannerStatus.textContent = `Planned ${result.queries.length} queries. Select one or several tabs above.`;
  } catch (error) {
    plannerStatus.textContent = `Planning failed: ${error.message}. Start this page with server.py.`;
  } finally {
    document.getElementById('plannerRun').disabled = false;
  }
});

addSchemaRow({ name: 'metrics', columns: 'ts:timestamp!, service:utf8!, region:utf8!, latency:float64!, bytes:int64!', timeIndex: 'ts' });
addSchemaRow({ name: 'hosts', columns: 'service:utf8!, region:utf8!' });
addPlannerRow({ name: 'count_by_service', schemas: ['metrics'] });
addPlannerRow({ name: 'avg_latency', schemas: ['metrics'], text: 'SELECT service, AVG(latency) FROM metrics GROUP BY service' });
addPlannerRow({
  name: 'q3',
  language: 'promql',
  text: 'topk(5, rate(http_requests_total[5m]))',
});
addPlannerRow({
  name: 'q6',
  language: 'sql',
  schemas: ['metrics', 'hosts'],
  text: 'SELECT metrics.service, COUNT(*) FROM metrics JOIN hosts ON metrics.service = hosts.service GROUP BY metrics.service',
});
addPlannerRow({
  name: 'q4',
  language: 'promql',
  text: 'topk(10, rate(http_requests_total[5m]))',
});
