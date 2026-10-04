// Query editor: plans PromQL queries through the local server's /api/plan
// (server.py runs `stage_pipeline`) and shows the resulting stage document.
(function () {
  const $ = (id) => document.getElementById(id);

  $('editorToggle').addEventListener('click', () => {
    const open = $('editor').hidden;
    $('editor').hidden = !open;
    $('editorToggle').setAttribute('aria-pressed', String(open));
  });

  let busy = false;
  $('editorForm').addEventListener('submit', async (event) => {
    event.preventDefault();
    if (busy) return;
    const queries = $('editorQueries').value.split('\n').map((q) => q.trim()).filter(Boolean);
    if (!queries.length) {
      $('editorStatus').textContent = 'Enter at least one PromQL query.';
      return;
    }
    const body = { queries, interval_ms: Number($('editorInterval').value) || 15000 };
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
      window.StageViewer.showDocument(result, `editor · ${queries.length} quer${queries.length === 1 ? 'y' : 'ies'}`);
      $('editorStatus').textContent = 'Planned.';
    } catch (err) {
      $('editorStatus').textContent = `Planning failed: ${err.message}`;
    } finally {
      busy = false;
    }
  });
})();
