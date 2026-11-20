import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { gunzipSync } from 'node:zlib';
test('production UI has a bounded single-document payload with no asset waterfall',async()=>{
 const html=await readFile(new URL('../dist/index.html',import.meta.url),'utf8');
 const gzip=await readFile(new URL('../dist/index.html.gz',import.meta.url));
 assert.equal(gunzipSync(gzip).toString(),html);
 assert.ok(gzip.length<25000,`Compressed UI exceeded 25KB: ${gzip.length}`);
 assert.doesNotMatch(html,/<script[^>]+src=|<link[^>]+rel="stylesheet"|fonts\.googleapis|fonts\.gstatic/);
 assert.match(html,/data:image\/png;base64,/);
});
