// Development and browser-test server. Production static serving and API proxying use Nginx.
import http from 'node:http';
import https from 'node:https';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
const files = new Map([['/', ['index.html', 'text/html']], ['/index.html', ['index.html', 'text/html']], ['/app.js', ['app.js', 'text/javascript']], ['/model.js', ['model.js', 'text/javascript']], ['/style.css', ['style.css', 'text/css']], ['/flame-logo.png', ['flame-logo.png', 'image/png']]]);
const directory=process.env.UI_STATIC_DIR === 'dist' ? 'dist' : 'static';
const server = http.createServer(async (request, response) => {
  const pathname = new URL(request.url, 'http://localhost').pathname;
  if (request.url.startsWith('/api/') && process.env.CONSOLE_API_TARGET) {
    const target = new URL(request.url, process.env.CONSOLE_API_TARGET);
    const proxy = (target.protocol === 'https:' ? https : http).request(target, { method: request.method, headers: request.headers }, upstream => {
      response.writeHead(upstream.statusCode, { 'Content-Type': upstream.headers['content-type'] || 'application/json' });
      upstream.pipe(response);
    });
    proxy.setTimeout(30000, () => proxy.destroy(new Error('API request timed out')));
    proxy.on('error', () => { if (!response.headersSent) response.writeHead(502, { 'Content-Type': 'application/json' }); response.end('{"error":"Console backend unavailable"}'); });
    request.pipe(proxy); return;
  }
  const file = files.get(pathname);
  if (!file) { response.writeHead(404); response.end(); return; }
  try {
    const data = await readFile(fileURLToPath(new URL(`./${directory}/${file[0]}`, import.meta.url)));
    response.writeHead(200, { 'Content-Type': file[1], 'Cache-Control': 'no-store' }); response.end(data);
  } catch { response.writeHead(500); response.end(); }
});
server.listen(Number(process.env.UI_PORT || 4173), '127.0.0.1');
