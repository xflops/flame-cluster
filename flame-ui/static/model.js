export const views = {
  nodes: { title: 'Nodes', description: 'The machines powering your cluster.', states: ['Unknown', 'Ready', 'Not ready'] },
  executors: { title: 'Executors', description: 'See where application services are running.', states: ['Unknown', 'Void', 'Idle', 'Binding', 'Bound', 'Unbinding', 'Releasing', 'Released'] },
  applications: { title: 'Applications', description: 'Your services and runtimes, ready for work.', states: ['Enabled', 'Disabled'] },
  sessions: { title: 'Sessions', description: 'Follow workloads from the queue to completion.', states: ['Open', 'Closed', 'Suspending', 'Suspended'] },
  settings: { title: 'Settings', description: 'Connection settings for your cluster.', states: [] },
};
export const taskStates = ['Pending', 'Running', 'Succeeded', 'Failed', 'Cancelled'];
export const escapeHtml = value => String(value ?? '—').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
export const number = value => Number.isFinite(Number(value)) ? Number(value) : 0;
export const formatNumber = value => number(value).toLocaleString();
export function bytes(value) {
  let size = number(value);
  if (!size) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit++; }
  return `${Number(size.toFixed(size < 10 ? 1 : 0))} ${units[unit]}`;
}
export const nameOf = row => row.metadata?.name || row.metadata?.id || 'Unnamed';
export const idOf = row => row.metadata?.id || row.metadata?.name || '';
export const stateOf = (row, kind) => row.status ? (kind === 'tasks' ? taskStates : views[kind]?.states)?.[row.status.state] || 'Unknown' : 'Unknown';
export const shim = value => ({ 0: 'Host', 1: 'Wasm', 3: 'CRI' }[value] || 'Unknown');
export function parseRoute(hash) {
  const view = hash.replace(/^#\/?/, '');
  return Object.hasOwn(views, view) ? view : 'applications';
}
