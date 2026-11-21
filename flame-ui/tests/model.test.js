import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseRoute, views, stateOf, bytes, escapeHtml } from '../static/model.js';

test('routes expose Flame resources and settings with applications as the default', () => {
  for (const view of Object.keys(views)) assert.equal(parseRoute(`#/${view}`), view);
  assert.deepEqual(Object.keys(views), ['nodes', 'executors', 'applications', 'sessions', 'settings']);
  for (const hash of ['', '#/clusters/example/nodes', '#/unknown', '#/sessions/tasks']) assert.equal(parseRoute(hash), 'applications');
});
test('state labels retain unknown and missing upstream status', () => {
  assert.equal(stateOf({status:{state:0}}, 'sessions'), 'Open');
  assert.equal(stateOf({status:{state:99}}, 'executors'), 'Unknown');
  assert.equal(stateOf({}, 'nodes'), 'Pending');
  assert.equal(stateOf({readiness:'Ready'}, 'nodes'), 'Ready');
  assert.equal(stateOf({readiness:'Disabled',status:{state:1}}, 'nodes'), 'Disabled');
  assert.equal(stateOf({status:{state:2}}, 'tasks'), 'Succeeded');
});
test('resource values are formatted and escaped without injecting markup', () => {
  assert.equal(bytes(1024 ** 3), '1 GiB');
  assert.equal(bytes(0), '0 B');
  assert.equal(escapeHtml('<img src="x">'), '&lt;img src=&quot;x&quot;&gt;');
});
