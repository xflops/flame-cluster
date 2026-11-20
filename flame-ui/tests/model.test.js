import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseRoute, views, stateOf, bytes, escapeHtml } from '../static/model.js';

test('routes expose only direct Flame resources with applications as the default', () => {
  for (const view of Object.keys(views)) assert.equal(parseRoute(`#/${view}`), view);
  for (const hash of ['', '#/settings', '#/clusters/example/nodes', '#/unknown', '#/sessions/tasks']) assert.equal(parseRoute(hash), 'applications');
});
test('state labels retain unknown and missing upstream status', () => {
  assert.equal(stateOf({status:{state:0}}, 'sessions'), 'Open');
  assert.equal(stateOf({status:{state:99}}, 'nodes'), 'Unknown');
  assert.equal(stateOf({}, 'nodes'), 'Unknown');
  assert.equal(stateOf({status:{state:2}}, 'tasks'), 'Succeeded');
});
test('resource values are formatted and escaped without injecting markup', () => {
  assert.equal(bytes(1024 ** 3), '1 GiB');
  assert.equal(bytes(0), '0 B');
  assert.equal(escapeHtml('<img src="x">'), '&lt;img src=&quot;x&quot;&gt;');
});
