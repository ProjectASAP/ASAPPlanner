// Query editor: plans PromQL or SQL queries through the local server's
// /api/plan (server.py runs `stage_pipeline`) and shows the resulting stage
// document. SQL queries read the tables declared one JSON object per line.
(function () {
  const $ = (id) => document.getElementById(id);

  $('editorToggle').addEventListener('click', () => {
    const open = $('editor').hidden;
    $('editor').hidden = !open;
    $('editorToggle').setAttribute('aria-pressed', String(open));
  });

  // Each language keeps its own queries while the other is shown.
  let language = 'promql';
  const drafts = { sql: 'SELECT COUNT(DISTINCT src_ip) FROM flows' };
  const LABELS = { promql: 'PromQL', sql: 'SQL' };
  const setLanguage = (next) => {
    if (next === language) return;
    drafts[language] = $('editorQueries').value;
    $('editorQueries').value = drafts[next];
    language = next;
    $('editorPromql').setAttribute('aria-pressed', String(next === 'promql'));
    $('editorSql').setAttribute('aria-pressed', String(next === 'sql'));
    $('editorTablesField').hidden = next !== 'sql';
    $('editorIntervalField').hidden = next === 'sql';
  };
  $('editorPromql').addEventListener('click', () => setLanguage('promql'));
  $('editorSql').addEventListener('click', () => setLanguage('sql'));

  const lines = (id) => $(id).value.split('\n').map((line) => line.trim()).filter(Boolean);

  let busy = false;
  $('editorForm').addEventListener('submit', async (event) => {
    event.preventDefault();
    if (busy) return;
    const queries = lines('editorQueries');
    if (!queries.length) {
      $('editorStatus').textContent = `Enter at least one ${LABELS[language]} query.`;
      return;
    }
    const body = { language, queries, interval_ms: Number($('editorInterval').value) || 15000 };
    if (language === 'sql') {
      try {
        body.tables = lines('editorTables').map((line, i) => {
          try { return JSON.parse(line); } catch (err) { throw new Error(`table line ${i + 1}: ${err.message}`); }
        });
      } catch (err) {
        $('editorStatus').textContent = err.message;
        return;
      }
    }
    if ($('editorEpsilon').value) body.epsilon = Number($('editorEpsilon').value);
    if ($('editorDelta').value) body.delta = Number($('editorDelta').value);
    busy = true;
    $('editorStatus').textContent = 'Planning…';
    try {
      const response = await fetch('/api/plan', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      const result = await response.json().catch(() => ({ error: `HTTP ${response.status}` }));
      if (!response.ok) throw new Error(result.error || `HTTP ${response.status}`);
      window.StageViewer.showDocument(result, `editor · ${queries.length} ${LABELS[language]} quer${queries.length === 1 ? 'y' : 'ies'}`);
      $('editorStatus').textContent = 'Planned.';
    } catch (err) {
      $('editorStatus').textContent = `Planning failed: ${err.message}`;
    } finally {
      busy = false;
    }
  });
})();
