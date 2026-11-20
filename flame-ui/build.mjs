import { build, transform } from 'esbuild';
import sharp from 'sharp';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { gzipSync } from 'node:zlib';
import { fileURLToPath } from 'node:url';
const root=fileURLToPath(new URL('./',import.meta.url));
const [javascript,css,images,source]=await Promise.all([
  build({entryPoints:[`${root}static/app.js`],bundle:true,minify:true,format:'esm',target:'es2022',write:false,legalComments:'none'}),
  readFile(`${root}static/style.css`,'utf8').then(source=>transform(source,{loader:'css',minify:true})),
  Promise.all([96,32].map(size=>sharp(`${root}static/flame-logo.png`).resize({width:size,height:size,fit:'inside',withoutEnlargement:true}).png({compressionLevel:9}).toBuffer())),
  readFile(`${root}static/index.html`,'utf8'),
]);
// A single compressed document avoids an asset dependency waterfall on high-RTT
// links. Public code/logo are inlined; live data and credentials are never baked in.
const [logo,favicon]=images;
const html=source.replace('<link rel="stylesheet" href="/style.css">',()=>`<style>${css.code}</style>`)
  .replace('<script type="module" src="/app.js"></script>',()=>`<script type="module">${javascript.outputFiles[0].text.replace(/<\/script/gi,'<\\/script')}</script>`)
  .replace('href="/flame-logo.png"',`href="data:image/png;base64,${favicon.toString('base64')}"`)
  .replace('src="/flame-logo.png"',`src="data:image/png;base64,${logo.toString('base64')}"`);
await mkdir(`${root}dist`,{recursive:true});
await writeFile(`${root}dist/index.html`,html);
const gzip=gzipSync(html,{level:9});
await writeFile(`${root}dist/index.html.gz`,gzip);
console.log(`UI build: ${Buffer.byteLength(html)} bytes HTML, ${gzip.length} bytes gzip, ${logo.length} bytes logo`);
