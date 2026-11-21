import { views, escapeHtml as e, number as n, formatNumber as f, bytes, nameOf, idOf, stateOf, shim, parseRoute } from './model.js';
const $ = selector => document.querySelector(selector);
const paths = {
  settings: '<path d="M4 7h16M4 17h16"/><circle cx="9" cy="7" r="3"/><circle cx="15" cy="17" r="3"/>',
  applications: '<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/>',
  sessions: '<rect x="4" y="3" width="16" height="18" rx="2"/><path d="M8 8h8M8 12h8M8 16h5"/>',
  nodes: '<rect x="3" y="3" width="18" height="7" rx="2"/><rect x="3" y="14" width="18" height="7" rx="2"/><path d="M7 6.5h.01M7 17.5h.01M12 6.5h5M12 17.5h5"/>',
  executors: '<rect x="6" y="6" width="12" height="12" rx="2"/><path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4"/><path d="m10 10 4 2-4 2z"/>',
};
const icon = kind => `<svg viewBox="0 0 24 24" aria-hidden="true">${paths[kind] || paths.applications}</svg>`;
const state = { view: parseRoute(location.hash), connections: null, connectionId: null, connection: null, join: null, downloading: false, downloadStatus: '', pages: {}, filter: '', page: 0, auto: true, loading: false, errors: {}, lastUpdate: null, drawer: null, action: null };
let catalogController, pollController, taskController, actionController, downloadController, revision = 0;
const pageSize = 12;
const apiBase = () => `/api/connections/${encodeURIComponent(state.connectionId)}`;
const pageCache = new Map();
function resourceUrl() {
  return `${apiBase()}/${state.view}?${new URLSearchParams({ page: state.page + 1, pageSize, state: state.filter })}`;
}
async function loadConnections() {
  if (catalogController) return;
  const controller = new AbortController();
  catalogController = controller;
  state.loading = true; render();
  let connections;
  try {
    const data = await json('/api/connections', AbortSignal.any([controller.signal, AbortSignal.timeout(15000)]));
    if (!Array.isArray(data.items) || data.items.some(item => !item || typeof item.id !== 'string' || !item.id)
      || new Set(data.items.map(item => item.id)).size !== data.items.length) throw new Error('Invalid Flame connection configuration.');
    if (!data.items.length) throw new Error('No Flame connections are configured.');
    connections = data.items;
    delete state.errors.connections;
  } catch (error) {
    state.errors.connections = error.name === 'TimeoutError' ? 'Loading Flame connections timed out. Refresh to retry.' : error.message;
  } finally {
    catalogController = null;
    state.loading = false;
  }
  if (connections) {
    state.connections = connections;
    selectConnection(connections[0].id);
  } else render();
}
function selectConnection(id) {
  if (id === state.connectionId || !state.connections?.some(connection => connection.id === id)) return;
  revision++; pollController?.abort(); taskController?.abort(); actionController?.abort(); downloadController?.abort();
  pageCache.clear();
  Object.assign(state, {connectionId: id, connection: null, join: null, downloading: false, downloadStatus: '', pages: {}, filter: '', page: 0, loading: false, errors: {}, lastUpdate: null, drawer: null, action: null});
  $('#details').close(); $('#detail-content').replaceChildren();
  render(); refresh();
}
function restorePage() {
  const cached = pageCache.get(resourceUrl());
  if (cached && Date.now() - cached.receivedAt.getTime() < 300000) {
    state.pages[state.view] = cached.data; state.lastUpdate = cached.receivedAt;
  } else delete state.pages[state.view];
}
const badge = label => {
  const tone = ['Ready', 'Enabled', 'Succeeded', 'Applied'].includes(label) ? 'green' : ['Open', 'Running', 'Bound'].includes(label) ? 'blue' : ['Pending', 'Binding', 'Suspending', 'Releasing', 'Deferred', 'Reconciling', 'Stale'].includes(label) ? 'orange' : ['Failed', 'Not ready'].includes(label) ? 'red' : '';
  return `<span class="badge ${tone}">${e(label)}</span>`;
};
const readiness = label => `<span class="readiness"><span class="ready-circle ${label === 'Ready' ? 'good' : label === 'Not ready' ? 'bad' : ''}" aria-hidden="true"></span>${e(label)}</span>`;
const date = value => {
  if (!value) return '—';
  const parsed = new Date(Number.isFinite(Number(value)) ? Number(value) : value);
  return Number.isNaN(parsed.getTime()) ? '—' : parsed.toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
};
const workerRuntimes = row => row.spec?.shims?.map(value => ({host:'Host',cri:'CRI'}[value] || value)).join(', ') || '—';
const yesNo = value => value === true ? 'Yes' : value === false ? 'No' : '—';
const cacheState = row => row.spec?.manageObjectCache === false ? 'Not managed' : row.status?.objectCache?.activeState;
const empty = (title, description, kind = state.view) => `<div class="empty">${icon(kind)}<h3>${e(title)}</h3><p>${e(description)}</p></div>`;
function table(headers, rows) { return `<div class="table-wrap"><table><thead><tr>${headers.map(header => `<th scope="col">${e(header)}</th>`).join('')}</tr></thead><tbody>${rows.join('')}</tbody></table></div>`; }
function resourceLink(row, index, subtitle = '', kind = state.view) {
  return `<button class="row-link" data-row="${index}"><span class="resource-icon">${icon(kind)}</span><span>${e(nameOf(row))}${subtitle ? `<span class="secondary">${e(subtitle)}</span>` : ''}</span></button>`;
}
function sessionProgress(status, detailed = false) {
  if (!status) return '<span class="dim">Progress unavailable</span>';
  const phases = [
    ['pending', 'Pending', status.pending], ['running', 'Running', status.running],
    ['succeeded', 'Succeeded', status.succeed], ['failed', 'Failed', status.failed],
    ['cancelled', 'Cancelled', status.cancelled],
  ].map(([key, label, value]) => ({key, label, count: Math.max(0, n(value))}));
  const total = phases.reduce((sum, phase) => sum + phase.count, 0);
  const complete = phases.filter(phase => ['succeeded','failed','cancelled'].includes(phase.key)).reduce((sum, phase) => sum + phase.count, 0);
  const percentage = total ? Math.round(complete / total * 100) : 0;
  const description = total ? phases.filter(phase => phase.key !== 'cancelled' || phase.count).map(phase => `${phase.label}: ${f(phase.count)}`).join(' · ') : 'No tasks';
  const segments = phases.filter(phase => phase.count).map(phase => `<span class="session-progress-segment ${phase.key}" style="width:${phase.count / total * 100}%" title="${phase.label}: ${f(phase.count)}" aria-hidden="true"></span>`).join('');
  const legend = detailed ? `<div class="session-progress-legend">${phases.filter(phase => phase.key !== 'cancelled' || phase.count).map(phase => `<span><i class="phase-dot ${phase.key}" aria-hidden="true"></i>${phase.label} <strong>${f(phase.count)}</strong></span>`).join('')}</div>` : '';
  return `<div class="session-progress${detailed ? ' detailed' : ''}"><div class="session-progress-track" role="progressbar" aria-label="Task completion" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${percentage}" aria-valuetext="${e(description)}" title="${e(description)}">${segments}</div><div class="session-progress-caption"><span>${total ? `${f(complete)} / ${f(total)} completed` : 'No tasks'}</span><span>${percentage}%</span></div>${legend}</div>`;
}

const sessionCommands = {suspend:'Suspend',close:'Close',resume:'Resume',delete:'Delete'};
function sessionActions(row, index) {
  const allowed = {0:['suspend','close'],1:['delete'],3:['resume']}[row.status?.state] || [];
  return `<div class="session-actions"><button class="button small subtle" data-row="${index}">View</button>${allowed.map(action => `<button class="button small" data-action="${action}" data-index="${index}" ${state.action ? 'disabled' : ''}>${sessionCommands[action]}</button>`).join('')}</div>`;
}
async function actOnSession(row, action, connectionId) {
  if (state.action || !connectionId || connectionId !== state.connectionId) return;
  const previousRevision = revision;
  const id = idOf(row);
  if (['close','delete'].includes(action) && !confirm(`${sessionCommands[action]} session “${id}”?${action === 'delete' ? ' Its task history will be removed.' : ' It will no longer accept tasks.'}`)) return;
  if (revision !== previousRevision || state.connectionId !== connectionId || state.action) return;
  revision++; pollController?.abort(); state.loading = false;
  const generation = revision;
  const controller = new AbortController(); actionController = controller;
  state.action = action; delete state.errors.action; render();
  try {
    const response = await fetch(`/api/connections/${encodeURIComponent(connectionId)}/sessions/${encodeURIComponent(id)}/actions/${action}`, {method:'POST',headers:{'Content-Type':'application/json'},body:'{}',signal:AbortSignal.any([controller.signal,AbortSignal.timeout(15000)])});
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error || 'Unable to change session. Refresh and retry.');
    if (generation === revision) {pageCache.clear(); state.pages = {};}
  } catch (error) {
    if (generation === revision) state.errors.action = error.name === 'TimeoutError' ? 'Action timed out. Refresh before retrying.' : error.message;
  } finally {
    if (generation === revision) {state.action = null; render(); refresh();}
  }
}

async function downloadClientConfig(connectionId) {
  if (state.downloading || state.view !== 'settings' || connectionId !== state.connectionId) return;
  const generation = revision, controller = new AbortController();
  downloadController = controller;
  state.downloading = true; state.downloadStatus = ''; render();
  const current = () => generation === revision && state.connectionId === connectionId && state.view === 'settings';
  try {
    const response = await fetch(`/api/connections/${encodeURIComponent(connectionId)}/flame.yaml`, {
      cache: 'no-store', headers: {Accept: 'application/yaml'}, signal: AbortSignal.any([controller.signal, AbortSignal.timeout(15000)]),
    });
    if (!response.ok) throw new Error('Client configuration is unavailable. Refresh and try again.');
    const bundle = await response.blob();
    if (!current() || controller.signal.aborted) return;
    const url = URL.createObjectURL(bundle), link = document.createElement('a');
    link.href = url; link.download = 'flame.yaml'; link.hidden = true;
    document.body.append(link); link.click(); link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    state.downloadStatus = 'Download started.';
  } catch (error) {
    if (current() && !controller.signal.aborted) state.downloadStatus = error.name === 'TimeoutError'
      ? 'Download timed out. Try again.' : 'Unable to download flame.yaml. Refresh and try again.';
  } finally {
    if (current()) {state.downloading = false; render();}
  }
}

function renderNavigation() {
  $('#navigation').innerHTML = Object.entries(views).map(([key, view]) => `<a class="nav-item ${state.view === key ? 'active' : ''}" href="#/${key}" ${state.view === key ? 'aria-current="page"' : ''}>${icon(key)}${view.title}</a>`).join('');
  const nav = $('#navigation'), selected = nav.querySelector('[aria-current="page"]');
  if (selected && nav.scrollWidth > nav.clientWidth) {
    const box = selected.getBoundingClientRect(), frame = nav.getBoundingClientRect();
    if (box.right > frame.right) nav.scrollLeft += box.right - frame.right + 8;
    else if (box.left < frame.left) nav.scrollLeft -= frame.left - box.left + 8;
  }
}
function render() {
  renderNavigation();
  const view = views[state.view];
  const selected = state.connections?.find(connection => connection.id === state.connectionId);
  const label = selected?.namespace || state.connection?.namespace || selected?.displayName || state.connection?.displayName || 'Flame';
  $('#connection-name').innerHTML = state.connections?.length
    ? state.connections.map(connection => `<option value="${e(connection.id)}">${e(connection.namespace || connection.displayName || connection.id)}</option>`).join('')
    : '<option>Loading Flames…</option>';
  if (state.connectionId) $('#connection-name').value = state.connectionId;
  $('#connection-name').disabled = !state.connections?.length;
  $('#connection-name').title = selected?.endpoint || '';
  $('#page-title').textContent = view.title;
  $('#page-description').textContent = view.description;
  $('#breadcrumb-detail').textContent = view.title;
  const settings = state.view === 'settings';
  $('#section-title').textContent = settings ? 'Cluster connection' : `All ${view.title.toLowerCase()}`;
  $('#row-count').hidden = settings;
  $('.filters').hidden = settings;
  $('.table-footer').hidden = settings;
  const filter = $('#status-filter');
  filter.innerHTML = '<option value="">All states</option>' + view.states.map(s => `<option>${e(s)}</option>`).join('');
  filter.value = state.filter;
  $('#auto-refresh').textContent = state.auto ? 'Auto-refresh on' : 'Auto-refresh off';
  $('#auto-refresh').setAttribute('aria-pressed', String(state.auto));
  $('#refresh').disabled = state.loading;
  $('#updated').textContent = state.lastUpdate ? `Updated ${state.lastUpdate.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })}` : 'Connecting…';
  const errors = Object.entries(state.errors), data = state.pages[state.view];
  $('#notice').hidden = !errors.length;
  $('#notice').textContent = errors.length ? `${errors.map(([key, value]) => `${key}: ${value}`).join(' · ')}${data ? ' Displaying the last successful data where available.' : ''}` : '';
  const dot = $('#connection-dot');
  dot.className = `live-dot ${errors.length ? 'bad' : state.lastUpdate ? 'good' : ''}`;
  dot.title = errors.length ? 'Some data is unavailable' : state.lastUpdate ? 'Connected' : 'Connecting';
  if (!state.connectionId) {
    $('#content').innerHTML = empty(state.errors.connections ? 'Unable to load Flame connections' : 'Loading Flames', state.errors.connections || 'Reading configured Flame connections…');
    $('#previous').disabled = true; $('#next').disabled = true;
    return;
  }
  if (settings) {
    $('#content').innerHTML = state.connection
      ? table(['Setting', 'Value'], [
        `<tr><td>Cluster</td><td>${e(label)}</td></tr>`,
        `<tr><td>FLM endpoint</td><td>${e(state.connection.endpoint)}</td></tr>`,
      ])
      : empty(errors.length ? 'Unable to load settings' : 'Connecting to Flame', errors.length ? 'Refresh to retry.' : 'Connection settings will appear here shortly.');
    const clientConfigAvailable = state.connection?.clientConfigAvailable ?? selected?.clientConfigAvailable;
    $('#content').insertAdjacentHTML('beforeend', `<section class="join-settings" aria-label="Client configuration"><h3>Client configuration</h3><p>${clientConfigAvailable
      ? `Download the Flame client configuration for ${e(selected?.namespace || selected?.displayName || state.connectionId)}.`
      : 'Client configuration is unavailable for this Flame.'}</p><button class="button" id="download-config" ${!clientConfigAvailable || state.downloading ? 'disabled' : ''}>${state.downloading ? 'Downloading…' : 'Download flame.yaml'}</button><p id="download-status" role="status">${e(state.downloadStatus)}</p></section>`);
    const downloadConnectionId = state.connectionId;
    $('#download-config').addEventListener('click', () => downloadClientConfig(downloadConnectionId));
    const join = state.join && Date.parse(state.join.expiresAt) > Date.now() ? state.join : null;
    $('#content').insertAdjacentHTML('beforeend', `<section class="join-settings" aria-label="Join a node"><h3>Join a node</h3><p>Run this command on the worker host. It downloads the installer; Flmlet installs and configures its Flame runtime.</p>${join
      ? `<pre><code id="join-command">${e(join.command)}</code></pre><p>Expires ${e(new Date(join.expiresAt).toLocaleString())}. Refresh to get the latest command.</p><button class="button" id="copy-join">Copy join command</button><span id="copy-status" role="status"></span>`
      : `<p>${e(state.errors.join || (state.loading ? 'Loading join command…' : 'Join command unavailable. Refresh to retry.'))}</p>`}</section>`);
    $('#copy-join')?.addEventListener('click', async () => {
      if (state.join !== join || state.view !== 'settings') return;
      const status = $('#copy-status');
      if (Date.parse(join.expiresAt) <= Date.now()) { status.textContent = 'Command expired. Refresh to get a new command.'; return; }
      try { await navigator.clipboard.writeText(join.command); status.textContent = 'Copied'; }
      catch { status.textContent = 'Select and copy the command above.'; }
    });
    return;
  }
  const rows = data?.items || [], total = data?.total ?? rows.length;
  $('#row-count').textContent = data ? f(total) : '—';
  $('#previous').disabled = state.page === 0 || !data;
  $('#next').disabled = !data?.hasNext;
  $('#page-number').textContent = `Page ${state.page + 1} of ${Math.max(1, Math.ceil(total / pageSize))}`;
  $('#table-caption').textContent = rows.length ? `Showing ${state.page * pageSize + 1}–${Math.min(state.page * pageSize + rows.length, total)} of ${f(total)} ${state.view}` : `0 ${state.view}`;
  if (!data) {
    $('#content').innerHTML = empty(errors.length ? 'Unable to load data' : 'Connecting to Flame', errors.length ? 'Refresh to retry.' : 'Live data will appear here shortly.'); return;
  }
  if (!rows.length) {
    $('#content').innerHTML = empty(state.filter ? 'No matching resources' : `No ${state.view} yet`, state.filter ? 'Try a different state filter.' : state.view === 'nodes' ? 'Nodes appear here when they enroll. Open Settings for the node join command.' : 'Resources will appear here when they are registered with Flame.'); return;
  }
  const displayed = rows;
  if (state.view === 'applications') {
    $('#content').innerHTML = table(['Application name', 'Status', 'Runtime', 'Created', 'Command', ''], displayed.map((row, i) => `<tr><td>${resourceLink(row, i, row.spec?.description)}</td><td>${badge(stateOf(row, 'applications'))}</td><td><span class="tag">${shim(row.spec?.shim)}</span></td><td>${date(row.status?.creation_time)}</td><td class="command mono" title="${e(row.spec?.command)}">${e(row.spec?.command)}</td><td class="chevron">›</td></tr>`));
  } else if (state.view === 'sessions') {
    $('#content').innerHTML = table(['Session', 'State', 'Application', 'Task progress', 'Created', 'Actions'], displayed.map((row, i) => `<tr><td>${resourceLink(row, i)}</td><td>${badge(stateOf(row, 'sessions'))}</td><td>${e(row.spec?.application)}</td><td>${sessionProgress(row.status)}</td><td>${date(row.status?.creation_time)}</td><td>${sessionActions(row, i)}</td></tr>`));
  } else if (state.view === 'nodes') {
    $('#content').innerHTML = table(['Node', 'Status', 'Address', 'FEM', 'Cache', 'Runtime', 'Executor limit', 'Heartbeat'], displayed.map((row, i) =>
      `<tr><td>${resourceLink(row, i, row.status?.hostname !== nameOf(row) ? row.status?.hostname : undefined)}</td><td>${readiness(stateOf(row, 'nodes'))}</td><td>${e(row.spec?.address)}</td><td>${e(row.status?.fem?.activeState)}</td><td>${e(cacheState(row))}</td><td>${e(workerRuntimes(row))}</td><td>${e(row.spec?.maxExecutors)}</td><td>${date(row.status?.lastHeartbeatTime)}</td></tr>`
    ));
  } else {
    $('#content').innerHTML = table(['Executor', 'State', 'Node', 'Application', 'Session', 'CPU', 'Memory', ''], displayed.map((row, i) => `<tr><td>${resourceLink(row, i, '', 'executors')}</td><td>${badge(stateOf(row, 'executors'))}</td><td>${e(row.spec?.node)}</td><td>${e(row.spec?.application)}</td><td class="mono">${e(row.status?.session_id)}</td><td>${f(row.spec?.resreq?.cpu)}</td><td>${bytes(row.spec?.resreq?.memory)}</td><td class="chevron">›</td></tr>`));
  }
  const displayedConnectionId = state.connectionId;
  document.querySelectorAll('[data-action]').forEach(button => button.addEventListener('click', () => actOnSession(displayed[Number(button.dataset.index)], button.dataset.action, displayedConnectionId)));
  document.querySelectorAll('[data-row]').forEach(button => button.addEventListener('click', () => openDetails(displayed[Number(button.dataset.row)])));
}
async function json(url, signal) {
  const response = await fetch(url, { signal, headers: { Accept: 'application/json' } });
  const data = await response.json();
  if (!response.ok) throw new Error(data.error || `Request failed (${response.status})`);
  return data;
}
async function refresh() {
  if (state.loading || state.action) return;
  if (!state.connectionId) { await loadConnections(); return; }
  const generation = revision, controller = new AbortController();
  pollController = controller;
  const timer = setTimeout(() => controller.abort(), 22000);
  state.loading = true; render();
  async function load(key, url, apply) {
    try {
      const data = await json(url, controller.signal);
      if (generation !== revision) return;
      apply(data); delete state.errors[key];
      if (key !== 'connection' && key !== 'join') {
        const receivedAt = new Date(); state.lastUpdate = receivedAt;
        pageCache.delete(url); pageCache.set(url, { data, receivedAt });
        if (pageCache.size > 24) pageCache.delete(pageCache.keys().next().value);
      }
      render();
    } catch (error) {
      if (generation !== revision) return;
      if (key === 'join') state.join = null;
      state.errors[key] = error.name === 'AbortError' ? 'Request timed out' : error.message;
      render();
    }
  }
  const view = state.view;
  await Promise.allSettled([
    load('connection', `${apiBase()}/connection`, data => { state.connection = data; }),
    ...(view === 'settings' ? [load('join', `${apiBase()}/join`, data => { state.join = data; })] : [load(view, resourceUrl(), data => { state.pages[view] = data; })]),
  ]);
  clearTimeout(timer);
  if (generation !== revision) return;
  state.loading = false;
  const data = state.pages[state.view];
  if (data && state.page && state.page * pageSize >= data.total) {
    state.page = Math.max(0, Math.ceil(data.total / pageSize) - 1); delete state.pages[state.view]; refresh(); return;
  }
  render();
}
function queryChanged() {
  revision++; pollController?.abort(); state.loading = false;
  restorePage(); render(); refresh();
}
const properties = rows => `<dl class="properties">${rows.map(([label, value]) => `<dt>${e(label)}</dt><dd>${e(value)}</dd>`).join('')}</dl>`;
function openDetails(row) {
  taskController?.abort();
  state.drawer = row;
  $('#detail-title').textContent = nameOf(row);
  $('#detail-kind').textContent = state.view.slice(0, -1).toUpperCase();
  const status = state.view === 'nodes' ? readiness(stateOf(row, state.view)) : badge(stateOf(row, state.view));
  let detail = status;
  if (state.view === 'applications') {
    detail += properties([['Runtime', shim(row.spec?.shim)], ['Description', row.spec?.description], ['Created', date(row.status?.creation_time)], ['Image', row.spec?.image], ['Package URL', row.spec?.url], ['Working directory', row.spec?.working_directory], ['Maximum instances', row.spec?.max_instances ?? 'Unlimited'], ['Labels', row.spec?.labels?.join(', ') || '—']]);
    detail += '<div class="drawer-section"><h3>Command</h3></div><pre>' + e([row.spec?.command, ...(row.spec?.arguments || [])].filter(Boolean).join(' ')) + '</pre>';
    if (row.spec?.schema) detail += '<div class="drawer-section"><h3>Schema</h3></div><pre>' + e(JSON.stringify(row.spec.schema, null, 2)) + '</pre>';
  } else if (state.view === 'nodes') {
    detail += properties([['Cluster', row.metadata?.namespace], ['Address', row.spec?.address], ['Hostname', row.status?.hostname], ['Runtimes', workerRuntimes(row)], ['Executor limit', row.spec?.maxExecutors], ['Python version', row.spec?.pythonVersion], ['Manage object cache', yesNo(row.spec?.manageObjectCache)], ['Registered with Flame', yesNo(row.status?.registered)], ['Cache ready', yesNo(row.status?.cacheReady)], ['Heartbeat', date(row.status?.lastHeartbeatTime)], ['Installed version', row.status?.installedVersion], ['Config hash', row.status?.configHash]]);
    detail += '<div class="drawer-section"><h3>Services</h3></div>' + table(['Service', 'State', 'Report'], [['FEM', row.status?.fem], ['Object cache', row.status?.objectCache]].map(([label, report]) => `<tr><td>${label}</td><td>${e(report?.activeState)}</td><td>${e([report?.subState, report?.message].filter(Boolean).join(' · ') || undefined)}</td></tr>`));
    if (row.status?.conditions?.length) detail += '<div class="drawer-section"><h3>Conditions</h3></div>' + table(['Condition', 'Status', 'Reason', 'Message'], row.status.conditions.map(condition => `<tr><td>${e(condition.type)}</td><td>${e(condition.status)}</td><td>${e(condition.reason)}</td><td>${e(condition.message)}</td></tr>`));
    if (!row.status || !Object.keys(row.status).length) detail += '<p class="detail-note">Worker status has not been reported yet.</p>';
  } else if (state.view === 'executors') {
    detail += properties([['Node', row.spec?.node], ['Application', row.spec?.application], ['Session', row.status?.session_id], ['Runtime', shim(row.spec?.shim)], ['CPU request', n(row.spec?.resreq?.cpu)], ['Memory request', bytes(row.spec?.resreq?.memory)], ['GPU request', n(row.spec?.resreq?.gpu)]]);
  } else {
    detail += properties([['Application', row.spec?.application], ['Created', date(row.status?.creation_time)], ['Completed', date(row.status?.completion_time)], ['Priority', n(row.spec?.priority)], ['CPU request', n(row.spec?.resreq?.cpu)], ['Memory request', bytes(row.spec?.resreq?.memory)]]);
    detail += `${sessionProgress(row.status, true)}<div class="drawer-section"><h3>Tasks</h3><button class="button small" id="refresh-tasks">Refresh tasks</button></div><div id="tasks"></div>`;
    if (row.status?.events?.length) detail += '<div class="drawer-section"><h3>Session events</h3></div><pre>' + e(row.status.events.map(event => `${date(event.creation_time)}  ${event.message || event.code}`).join('\n')) + '</pre>';
  }
  $('#detail-content').innerHTML = detail;
  if (!$('#details').open) $('#details').showModal();
  if (state.view === 'sessions') {
    $('#refresh-tasks').addEventListener('click', () => loadTasks(row));
    loadTasks(row);
  }
}
async function loadTasks(row, page = 1) {
  taskController?.abort();
  taskController = new AbortController();
  const controller = taskController;
  const signal = controller.signal;
  const target = $('#tasks');
  target.innerHTML = '<div class="empty"><div class="spinner"></div><p>Loading tasks…</p></div>';
  const timer = setTimeout(() => controller.abort(), 23000);
  try {
    const data = await json(`${apiBase()}/sessions/${encodeURIComponent(idOf(row))}/tasks?page=${page}&pageSize=${pageSize}`, signal);
    if (signal.aborted || state.drawer !== row) return;
    const tasks = data.items;
    target.innerHTML = tasks.length ? table(['Task', 'State', 'Created', 'Completed'], tasks.map(task => `<tr><td class="mono">${e(idOf(task))}</td><td>${badge(stateOf(task, 'tasks'))}</td><td>${date(task.status?.creation_time)}</td><td>${date(task.status?.completion_time)}</td></tr>`)) : empty(page === 1 ? 'No tasks yet' : 'No tasks on this page', page === 1 ? 'Tasks submitted to this session will appear here.' : 'Use Previous tasks to return to an earlier page.', 'sessions');
    target.insertAdjacentHTML('beforeend', `<div class="pagination task-pagination"><button class="button small" id="tasks-previous" ${page === 1 ? 'disabled' : ''}>← Previous tasks</button><span>Page ${page}</span><button class="button small" id="tasks-next" ${data.hasNext ? '' : 'disabled'}>Next tasks →</button></div>`);
    $('#tasks-previous').addEventListener('click', () => loadTasks(row, page - 1));
    $('#tasks-next').addEventListener('click', () => loadTasks(row, page + 1));
  } catch (error) {
    if (taskController === controller && state.drawer === row) target.innerHTML = `<div class="notice">${e(error.name === 'AbortError' ? 'Task request timed out. Use Refresh tasks to retry.' : error.message)}</div>`;
  } finally { clearTimeout(timer); }
}
function route() {
  revision++; pollController?.abort(); taskController?.abort(); actionController?.abort(); downloadController?.abort();
  state.view = parseRoute(location.hash);
  if (location.hash !== `#/${state.view}`) history.replaceState(null, '', `${location.pathname}${location.search}#/${state.view}`);
  Object.assign(state, {loading: false, filter: '', page: 0, errors: {}, drawer: null, action: null, join: null, downloading: false, downloadStatus: ''});
  restorePage(); $('#details').close();
  document.title = `${views[state.view].title} · Flame Console`;
  render(); refresh();
}
$('#status-filter').addEventListener('change', event => { state.filter = event.target.value; state.page = 0; queryChanged(); });
$('#connection-name').addEventListener('change', event => selectConnection(event.target.value));
$('#refresh').addEventListener('click', refresh);
$('#auto-refresh').addEventListener('click', () => { state.auto = !state.auto; render(); });
$('#previous').addEventListener('click', () => { state.page--; queryChanged(); });
$('#next').addEventListener('click', () => { state.page++; queryChanged(); });
$('#close-details').addEventListener('click', () => $('#details').close());
$('#details').addEventListener('close', () => { state.drawer = null; taskController?.abort(); });
$('#details').addEventListener('click', event => { if (event.target === $('#details') && event.clientX < $('#details').getBoundingClientRect().left) $('#details').close(); });
window.addEventListener('hashchange', route);
window.addEventListener('resize', renderNavigation);
setInterval(() => { if (state.auto && !document.hidden) refresh(); }, 15000);
route();
