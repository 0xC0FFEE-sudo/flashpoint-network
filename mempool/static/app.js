async function jfetch(url, opts = {}) {
  const headers = opts.headers || {};
  const key = localStorage.getItem('fpn_api_key');
  if (key && ['POST', 'DELETE', 'PUT', 'PATCH', undefined].includes((opts.method || 'GET').toUpperCase())) {
    // Send key on mutating endpoints (and allow on GET as harmless)
    headers['X-API-Key'] = key;
  }
  const res = await fetch(url, { ...opts, headers });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  return res.headers.get('content-type')?.includes('application/json') ? res.json() : res.text();
}

function qs(sel) { return document.querySelector(sel); }
function qsa(sel) { return Array.from(document.querySelectorAll(sel)); }

function setText(el, text) { el.textContent = text; }
function setJSON(el, obj) { el.textContent = JSON.stringify(obj, null, 2); }
function getApiKey() { return localStorage.getItem('fpn_api_key') || ''; }
function saveApiKey(v) { if (v) localStorage.setItem('fpn_api_key', v); else localStorage.removeItem('fpn_api_key'); }

async function onHealth() {
  try {
    const data = await jfetch('/health');
    setText(qs('#health-out'), `status=${data.status}`);
  } catch (e) {
    setText(qs('#health-out'), `error: ${e}`);
  }
}

async function onReset() {
  try {
    await jfetch('/reset', { method: 'DELETE' });
    await onList();
    setText(qs('#health-out'), 'reset ok');
  } catch (e) {
    setText(qs('#health-out'), `error: ${e}`);
  }
}

async function onSubmit() {
  const profit = parseFloat(qs('#profit').value || '0');
  const resources = qs('#resources').value.split(',').map(s => s.trim()).filter(Boolean);
  const client_id = qs('#client_id').value || null;
  try {
    const data = await jfetch('/intents', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ profit, resources, client_id }),
    });
    setJSON(qs('#submit-out'), data);
    await onList();
  } catch (e) {
    setText(qs('#submit-out'), `error: ${e}`);
  }
}

async function onBulk() {
  const raw = qs('#bulk-json').value;
  try {
    const intents = JSON.parse(raw);
    const data = await jfetch('/intents/bulk', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ intents }),
    });
    setText(qs('#bulk-out'), `created=${data.length}`);
    await onList();
  } catch (e) {
    setText(qs('#bulk-out'), `error: ${e}`);
  }
}

function renderIntents(list) {
  const tbody = qs('#intents-table tbody');
  tbody.innerHTML = '';
  list.forEach((it, idx) => {
    const tr = document.createElement('tr');
    tr.innerHTML = `<td>${idx+1}</td><td>${it.profit.toFixed(2)}</td><td>${it.resources.join(', ')}</td><td>${it.client_id ?? ''}</td><td>${it.arrival_index ?? ''}</td>`;
    tbody.appendChild(tr);
  });
  setText(qs('#count-out'), `${list.length} intents`);
}

async function onList() {
  try {
    const data = await jfetch('/intents');
    renderIntents(data);
  } catch (e) {
    setText(qs('#count-out'), `error: ${e}`);
  }
}

async function onCompare() {
  try {
    const data = await jfetch('/compare');
    setJSON(qs('#compare-out'), data);
  } catch (e) {
    setText(qs('#compare-out'), `error: ${e}`);
  }
}

function renderSelected(selected) {
  const tbody = qs('#selected-table tbody');
  tbody.innerHTML = '';
  selected.forEach(it => {
    const tr = document.createElement('tr');
    tr.innerHTML = `<td>${it.profit.toFixed(2)}</td><td>${it.trader_profit.toFixed(2)}</td><td>${it.fee_share.toFixed(2)}</td><td>${it.resources.join(', ')}</td>`;
    tbody.appendChild(tr);
  });
}

async function onBuild() {
  const algorithm = qs('#algo').value;
  const fee_rate = parseFloat(qs('#fee_rate').value || '0');
  try {
    const data = await jfetch(`/build_block?algorithm=${encodeURIComponent(algorithm)}&fee_rate=${fee_rate}`);
    const summary = {
      algorithm: data.algorithm,
      fee_rate: data.fee_rate,
      baseline_fifo_profit: data.baseline_fifo_profit,
      total_profit: data.total_profit,
      surplus: data.surplus,
      sequencer_fee: data.sequencer_fee,
    };
    setJSON(qs('#block-summary'), summary);
    renderSelected(data.selected);
  } catch (e) {
    setText(qs('#block-summary'), `error: ${e}`);
  }
}

async function onMetrics() {
  try {
    const data = await jfetch('/metrics');
    setJSON(qs('#metrics-out'), data);
  } catch (e) {
    setText(qs('#metrics-out'), `error: ${e}`);
  }
}

async function onSeed() {
  const count = parseInt(qs('#seed-count').value || '0', 10);
  try {
    const data = await jfetch('/seed', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ count }),
    });
    setText(qs('#seed-out'), `created=${data.length}`);
    await onList();
    await onMetrics();
  } catch (e) {
    setText(qs('#seed-out'), `error: ${e}`);
  }
}

function bind() {
  qs('#btn-health').addEventListener('click', onHealth);
  qs('#btn-reset').addEventListener('click', onReset);
  qs('#btn-submit').addEventListener('click', onSubmit);
  qs('#btn-bulk').addEventListener('click', onBulk);
  qs('#btn-list').addEventListener('click', onList);
  qs('#btn-compare').addEventListener('click', onCompare);
  qs('#btn-build').addEventListener('click', onBuild);
  qs('#btn-metrics').addEventListener('click', onMetrics);
  qs('#btn-seed').addEventListener('click', onSeed);
  qs('#btn-save-key').addEventListener('click', () => {
    saveApiKey(qs('#api_key').value.trim());
    connectWS(true);
  });
}

let ws;
function setWSStatus(ok) {
  const el = qs('#ws-status');
  if (!el) return;
  el.textContent = ok ? 'WS: connected' : 'WS: disconnected';
  el.classList.toggle('ok', !!ok);
}

function connectWS(force = false) {
  try { if (ws && !force) return; if (ws) ws.close(); } catch {}
  ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws');
  ws.onopen = () => setWSStatus(true);
  ws.onclose = () => setWSStatus(false);
  ws.onerror = () => setWSStatus(false);
  ws.onmessage = (ev) => {
    try {
      const msg = JSON.parse(ev.data);
      if (msg.type === 'intent_submitted' || msg.type === 'intents_bulk_submitted' || msg.type === 'reset') {
        onList();
        onMetrics();
      }
      if (msg.type === 'block_built') {
        onMetrics();
      }
    } catch {}
  };
}

window.addEventListener('DOMContentLoaded', async () => {
  bind();
  qs('#api_key').value = getApiKey();
  connectWS();
  await onHealth();
  await onList();
  await onMetrics();
});
