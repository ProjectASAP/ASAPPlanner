const plannerPanel = document.getElementById('plannerPanel');
const plannerRows = document.getElementById('plannerRows');
const plannerStatus = document.getElementById('plannerStatus');

function addPlannerRow(query = {}) {
  const row = document.createElement('div');
  row.className = 'plannerRow';
  row.innerHTML = `
    <input class="plannerInclude" type="checkbox" checked title="Include in workload" />
    <input class="plannerName" value="${escapeHtml(query.name || `q${plannerRows.children.length + 1}`)}" aria-label="Query name" />
    <select class="plannerLanguage" aria-label="Language"><option value="sql">SQL</option><option value="promql">PromQL</option></select>
    <textarea class="plannerText" aria-label="Query text" rows="2">${escapeHtml(query.text || 'SELECT service, COUNT(*) FROM metrics GROUP BY service')}</textarea>
    <button class="btn plannerRemove" type="button">Remove</button>`;
  row.querySelector('.plannerLanguage').value = query.language || 'sql';
  row.querySelector('.plannerRemove').addEventListener('click', () => row.remove());
  plannerRows.appendChild(row);
}

document.getElementById('plannerToggle').addEventListener('click', () => plannerPanel.classList.toggle('visible'));
document.getElementById('plannerAdd').addEventListener('click', () => addPlannerRow({ text: '' }));
document.getElementById('plannerRun').addEventListener('click', async () => {
  const rows = Array.from(plannerRows.querySelectorAll('.plannerRow')).filter((row) => row.querySelector('.plannerInclude').checked);
  const queries = rows.map((row) => ({
    name: row.querySelector('.plannerName').value,
    language: row.querySelector('.plannerLanguage').value,
    text: row.querySelector('.plannerText').value,
  }));
  plannerStatus.textContent = 'Running pre-ASAP lowering and ASAP-aware mapping…';
  document.getElementById('plannerRun').disabled = true;
  try {
    const response = await fetch('/api/plan', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ queries, epsilon: Number(document.getElementById('plannerEpsilon').value) }),
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

addPlannerRow({ name: 'count_by_service' });
addPlannerRow({ name: 'avg_latency', text: 'SELECT service, AVG(latency) FROM metrics GROUP BY service' });
addPlannerRow({
  name: 'q3',
  language: 'promql',
  text: 'topk(5, rate(http_requests_total[5m]))',
});
addPlannerRow({
  name: 'q4',
  language: 'promql',
  text: 'topk(10, rate(http_requests_total[5m]))',
});
