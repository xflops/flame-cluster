import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const app = (name,state=0) => ({metadata:{name},spec:{shim:3,command:'/usr/local/flame/bin/flmping-service',description:'Application service'},status:{state,creation_time:1794614400000}});
const inventory = {
  applications:[app('flmping'),app('flmrun',1),...Array.from({length:13},(_,i)=>app(`python-${i}`))],
  nodes:[{metadata:{name:'flm-worker-01',namespace:'training-ns',generation:1},spec:{clusterRef:'training-1',address:'192.168.3.8',shims:['host','cri'],maxExecutors:10,pythonVersion:'3.12',manageObjectCache:true},readiness:'Ready',status:{pciDevices:[{address:'0000:03:00.0',vendor_id:'10de',device_id:'1234',class_id:'030000',driver:'',numa_node:0}],capacity:{gpu:0,cpu:8,memory:16*1024**3},observedGeneration:1,hostname:'flm-worker-01',registered:true,cacheReady:true,lastHeartbeatTime:'2026-11-14T00:00:00Z',installedVersion:'0.8.0',configHash:'ready-worker-hash',fem:{activeState:'active',subState:'running',message:'success'},objectCache:{activeState:'active',subState:'running',message:'success'},conditions:[{type:'Ready',status:'True',reason:'Registered',message:'Worker registered and services active'}]}}],
  sessions:[{metadata:{id:'session-42'},spec:{application:'flmping',priority:1},status:{state:0,running:2,pending:3,succeed:16,failed:0,creation_time:1794614400000}}],
  executors:[{metadata:{id:'executor-01'},spec:{node:'flm-worker-01',application:'flmping',shim:3,resreq:{cpu:2,memory:1024**3}},status:{state:4,session_id:'session-42'}}],
};
const stateNames = {nodes:['Pending','Ready','Not ready','Disabled','Deleting'],applications:['Enabled','Disabled'],sessions:['Open','Closed','Suspending','Suspended'],executors:['Unknown','Void','Idle','Binding','Bound','Unbinding','Releasing','Released']};
const clusterConfiguration = (name, endpoint) => ({
  cluster:{name,endpoint,tls:{cert_file:'/etc/flame/control-plane-tls/tls.crt',key_file:'/etc/flame/control-plane-tls/tls.key',ca_file:'/etc/flame/control-plane-tls/ca.crt'},resreq:'cpu=1,mem=128m',policies:['fairshare'],storage:'fs:///var/lib/flame/session',schedule_interval:100,executors:{shims:['host','cri']},limits:{node:{max_executors:10}}},
  cache:{endpoint:`grpc://${name}-cache-bootstrap:9090`},
});
const connection = {id:'training',endpoint:'http://flm:8080',displayName:'Training Flame',clientConfigAvailable:true,configuration:clusterConfiguration('training-1','https://training-1-session-manager:8080')};
const hostedJoin = "curl -fsSL https://xflops.io/cluster/setup.sh | sudo sh -s -- --join 'current-token'";
const clientBundle = id => `current-context: ${id}\r\ncontexts:\r\n  - name: ${id}\r\n    cluster:\r\n      endpoint: https://${id}:30443\r\n      tls:\r\n        key: PRIVATE-KEY-FIXTURE-${id}\r\n# 火焰\r\n`;
const scoped = resource => `/api/connections/training/${resource}`;
async function fixture(page, options={}) {
  const connections=options.connections || [connection];
  const inventories=Object.fromEntries(connections.map(item=>[item.id,structuredClone(options.inventories?.[item.id] || inventory)]));
  await page.route('**/api/**',async route=>{
    const url=new URL(route.request().url()),segments=url.pathname.split('/'),resource=segments.at(-1);
    if(url.pathname==='/api/connections') {
      if(options.catalogFail) return route.fulfill({status:503,json:{error:'Flame configuration is unavailable.'}});
      return route.fulfill({json:{items:connections}});
    }
    const selected=connections.find(item=>item.id===segments[3]), data=inventories[selected?.id];
    if(!selected || segments[2]!=='connections') return route.fulfill({status:404,json:{error:'Unknown connection'}});
    if(options.wait?.[`${selected.id}/${resource}`]) await options.wait[`${selected.id}/${resource}`];
    if(resource==='connection') return route.fulfill({json:selected});
    if(resource==='flame.yaml') return options.downloadFail
      ? route.fulfill({status:503,contentType:'application/yaml',body:'PRIVATE-KEY-ERROR-BODY'})
      : route.fulfill({body:clientBundle(selected.id),headers:{'Content-Type':'application/yaml','Content-Disposition':'attachment; filename="flame.yaml"','Cache-Control':'no-store'}});
    if(resource==='join') return options.joinFail
      ? route.fulfill({status:503,json:{error:'Node join is not configured.'}})
      : route.fulfill({json:{command:options.joinCommands?.[selected.id] || hostedJoin,expiresAt:new Date(Date.now()+(options.joinExpired ? -60000 : 900000)).toISOString()}});
    if(route.request().method()==='POST') {
      if(options.actionFail) return route.fulfill({status:409,json:{error:'Session state changed. Refresh and retry.'}});
      if(resource==='delete') data.sessions=[];
      else data.sessions[0].status.state={close:1,suspend:2,resume:0}[resource];
      return route.fulfill({json:{session:data.sessions[0]||null}});
    }
    const currentPage=Number(url.searchParams.get('page')||1),pageSize=Number(url.searchParams.get('pageSize')||12);
    if(resource==='tasks') return route.fulfill({json:{items:[{metadata:{id:options.taskPrefix ? `${selected.id}-task-${currentPage}` : `task-${currentPage}`},status:{state:2,creation_time:1794614400000}}],page:currentPage,pageSize,total:null,hasNext:currentPage===1}});
    if(options.fail) return route.fulfill({status:502,json:{error:'FLM is unavailable'}});
    if(!Object.hasOwn(data,resource)) return route.fulfill({status:404,json:{error:'Unknown resource'}});
    if(options.delays?.[resource]) await new Promise(resolve=>setTimeout(resolve,options.delays[resource]));
    const filter=url.searchParams.get('state'),rows=data[resource].filter(row=>!filter||(resource==='nodes' ? row.readiness : stateNames[resource][row.status.state])===filter);
    return route.fulfill({json:{items:rows.slice((currentPage-1)*pageSize,currentPage*pageSize),total:rows.length,page:currentPage,pageSize,hasNext:currentPage*pageSize<rows.length}});
  });
}
test('root is the console and reads only its active Flame resource',async({page})=>{
  const paths=[];page.on('request',r=>{if(new URL(r.url()).pathname.startsWith('/api/'))paths.push(new URL(r.url()).pathname)});
  await fixture(page);await page.goto('/');
  await expect(page).toHaveURL(/\/#\/dashboard$/);
  await expect(page.getByRole('heading',{name:'Dashboard',exact:true,level:1})).toBeVisible();
  await expect(page.getByRole('img',{name:'Nodes: 1 ready, 0 not ready, 1 total'})).toBeVisible();
  await expect(page.locator('#nav-label')).toHaveCount(0);
  await expect(page.locator('#connection-name')).toHaveText('Training Flame');
  const clusters = page.getByRole('combobox', {name:'Cluster', exact:true});
  await expect(clusters).toBeEnabled();
  await expect(clusters.locator('option')).toHaveText(['Training Flame']);
  await expect(clusters).toHaveValue('training');
  expect(paths.every(p=>['/api/connections',scoped('connection'),scoped('nodes')].includes(p))).toBe(true);
  await expect(page.locator('#navigation a')).toHaveText(['Dashboard','Nodes','Executors','Applications','Sessions','Settings']);
  expect((await page.request.get('/dashboard')).status()).toBe(404);
  expect((await page.request.get('/console')).status()).toBe(404);
  await page.getByRole('link',{name:'Applications',exact:true}).click();
  await expect(page.getByRole('heading',{name:'Applications',exact:true,level:1})).toBeVisible();
  await expect(clusters.locator('option')).toHaveText(['Training Flame']);
  await expect(clusters).toHaveValue('training');
  await expect(page.locator('#breadcrumb-home')).toHaveAttribute('href','/');
  await page.locator('#breadcrumb-home').click();
  await expect(page).toHaveURL(/\/#\/dashboard$/);
  await expect(page.getByRole('heading',{name:'Dashboard',exact:true,level:1})).toBeVisible();
});
const cpuUsage=page=>page.getByRole('region',{name:'CPU usage',exact:true}).locator('.metric-value');
test('dashboard sums reported capacity across pages and keeps cluster usage separate',async({page})=>{
  const now=Date.parse('2030-01-01T00:00:00Z'),gib=1024**3;await page.clock.install({time:new Date(now)});
  const nodes=Array.from({length:101},(_,index)=>({metadata:{name:`worker-${index}`},readiness:index===100?'Pending':'Ready',status:{lastHeartbeatTime:new Date(now).toISOString(),capacity:index===0?{gpu:0,cpu:8,memory:gib}:index===100?{cpu:16,memory:3*gib}:undefined,usage:index===0?{cpuPercent:0,memory:{usedBytes:gib,totalBytes:8*gib}}:index===100?{cpuPercent:50,memory:{usedBytes:3*gib,totalBytes:8*gib},gpuDevices:[{pciAddress:'0000:01:00.0',utilizationPercent:0}]}:null}}));
  const reads=[];page.on('request',request=>{const url=new URL(request.url());if(url.pathname===scoped('nodes'))reads.push(url.searchParams.get('page'))});
  await fixture(page,{inventories:{training:{...inventory,nodes}}});await page.goto('/');
  await expect(page.getByRole('img',{name:'Nodes: 100 ready, 1 not ready, 101 total'})).toBeVisible();expect(reads).toEqual(['1','2']);
  await expect(page.locator('.usage-metric .metric-value')).toHaveText(['25%','4 GiB / 16 GiB','0%']);
  await expect(page.getByRole('region',{name:'Memory usage'})).toContainText('Used / total · Reporting 2/101 nodes');
  await expect(page.getByRole('region',{name:'GPU usage'})).toContainText('1 reporting GPU · Reporting 1/101 nodes');
  await expect(page.getByRole('region',{name:'Current usage'}).getByRole('row')).toHaveCount(0);
  await expect(page.locator('.dashboard-summary .dashboard-capacity .metric-value')).toHaveText(['0','24','4 GiB']);
  await expect(page.locator('.dashboard-summary .dashboard-capacity p')).toHaveText(['Reporting 1/101 nodes','Reporting 2/101 nodes','Reporting 2/101 nodes']);
  await expect(page.locator('.dashboard-summary').getByRole('region',{name:'Current usage'})).toHaveCount(0);
  await expect(page.getByLabel('Filter by status')).toBeHidden();await expect(page.locator('.table-footer')).toBeHidden();
});
test('dashboard isolates each selected cluster and shows unknown readings as hyphens',async({page})=>{
  const now=Date.parse('2030-01-01T00:00:00Z');await page.clock.install({time:new Date(now)});
  const node=structuredClone(inventory.nodes[0]);node.status.lastHeartbeatTime=new Date(now).toISOString();node.status.usage={cpuPercent:8,gpuDevices:[{pciAddress:'0000:01:00.0',utilizationPercent:0}]};
  const missing={metadata:{name:'unreported'},readiness:'Pending'};
  await fixture(page,{...multiple,inventories:{training:{...inventory,nodes:[node]},production:{...productionInventory,nodes:[missing]}}});await page.goto('/');
  await expect(page.locator('.usage-metric .metric-value')).toHaveText(['8%','-','0%']);
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');
  await expect(page.getByRole('img',{name:'Nodes: 0 ready, 1 not ready, 1 total'})).toBeVisible();
  await expect(page.locator('.usage-metric .metric-value')).toHaveText(['-','-','-']);
  await expect(page.locator('.dashboard-capacity .metric-value')).toHaveText(['-','-','-']);
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('training');
  await expect(cpuUsage(page)).toHaveText('8%');
});
test('dashboard refreshes current readings and refuses incomplete inventories',async({page})=>{
  const now=Date.parse('2030-01-01T00:00:00Z');await page.clock.install({time:new Date(now)});
  const node=structuredClone(inventory.nodes[0]);node.status.lastHeartbeatTime=new Date(now).toISOString();node.status.usage={cpuPercent:8};
  await fixture(page,{inventories:{training:{...inventory,nodes:[node]}}});await page.goto('/');
  await expect(cpuUsage(page)).toHaveText('8%');
  await page.unroute('**/api/**');node.status.usage.cpuPercent=12;await fixture(page,{inventories:{training:{...inventory,nodes:[node]}}});await page.clock.fastForward(15000);
  await expect(cpuUsage(page)).toHaveText('12%');
  await page.goto('/#/nodes');await page.route('**/api/connections/training/nodes?**',route=>route.fulfill({json:{items:[node],total:2,hasNext:false}}));await page.goto('/#/dashboard');
  await expect(page.locator('#notice')).toContainText('incomplete');await expect(page.locator('#notice')).toContainText('last successful data');
  await expect(cpuUsage(page)).toHaveText('12%');
});
test('cluster usage averages CPU and GPU samples and sums memory bytes',async({page})=>{
  const now=Date.parse('2030-01-01T00:00:00Z'),gib=1024**3;await page.clock.install({time:new Date(now)});
  const node=(name,readiness,age,usage)=>({metadata:{name},readiness,status:{lastHeartbeatTime:new Date(now-age).toISOString(),usage}});
  const nodes=[node('zero','Ready',0,{cpuPercent:0,memory:{usedBytes:gib,totalBytes:8*gib},gpuDevices:[{pciAddress:'0000:01:00.0',utilizationPercent:0}]}),
    node('pending','Pending',0,{cpuPercent:100,memory:{usedBytes:3*gib,totalBytes:16*gib},gpuDevices:[{pciAddress:'0000:01:00.0',utilizationPercent:60},{pciAddress:'0000:02:00.0',utilizationPercent:90}]}),
    node('stale','Ready',121000,{cpuPercent:100,gpuDevices:[{pciAddress:'0000:01:00.0',utilizationPercent:100}]}),node('future','Ready',-1000,{cpuPercent:100}),
    node('invalid','Ready',0,{cpuPercent:101,memory:{usedBytes:9,totalBytes:8},gpuDevices:[{pciAddress:'0000:01:00.0',utilizationPercent:null},{pciAddress:'unknown',utilizationPercent:50}]}),
    {metadata:{name:'missing-heartbeat'},readiness:'Ready',status:{usage:{cpuPercent:100}}}];
  await fixture(page,{inventories:{training:{...inventory,nodes}}});await page.goto('/');
  await expect(page.locator('.usage-metric .metric-value')).toHaveText(['50%','4 GiB / 24 GiB','50%']);
  await expect(page.getByRole('region',{name:'GPU usage'})).toContainText('3 reporting GPUs · Reporting 2/6 nodes');
  await page.getByRole('link',{name:'Nodes',exact:true}).click();await page.getByRole('button',{name:'pending',exact:true}).click();
  await expect(page.getByRole('region',{name:'Node resource usage'}).locator('dd')).toHaveText(['100%','3 GiB / 16 GiB','0000:01:00.0 · 60%; 0000:02:00.0 · 90%']);
});
test('cached current usage expires during an upstream failure',async({page})=>{
  const now=Date.parse('2030-01-01T00:00:00Z');await page.clock.install({time:new Date(now)});
  const node=structuredClone(inventory.nodes[0]);node.status.lastHeartbeatTime=new Date(now).toISOString();node.status.usage={cpuPercent:0,gpuDevices:[{pciAddress:'0000:01:00.0',utilizationPercent:0}]};
  await fixture(page,{inventories:{training:{...inventory,nodes:[node]}}});await page.goto('/');
  await expect(page.locator('.usage-metric .metric-value')).toHaveText(['0%','-','0%']);
  await page.unroute('**/api/**');await fixture(page,{fail:true});await page.clock.fastForward(121000);
  await expect(page.locator('#notice')).toContainText('last successful data');
  await expect(page.locator('.usage-metric .metric-value')).toHaveText(['-','-','-']);
});
test('settings shows the configured cluster without requesting a resource inventory',async({page})=>{
  const paths=[];page.on('request',r=>{if(new URL(r.url()).pathname.startsWith('/api/'))paths.push(new URL(r.url()).pathname)});
  await fixture(page);await page.goto('/#/settings');
  await expect(page.getByRole('heading',{name:'Settings',exact:true,level:1})).toBeVisible();
  await expect(page.locator('#content > .settings-card')).toHaveCount(3);
  await expect(page.locator('#content > .settings-card h3')).toHaveText(['Cluster configuration','Client configuration','Join a node']);
  await expect(page.locator('#content')).toContainText('Training Flame');
  await expect(page.locator('#content')).toContainText(connection.configuration.cluster.endpoint);
  await expect(page.getByLabel('Filter by status')).toBeHidden();
  await expect(page.getByRole('button',{name:'Next →',exact:true})).toBeHidden();
  await expect(page.locator('#join-command')).toHaveText(hostedJoin);
  await page.context().grantPermissions(['clipboard-read','clipboard-write']);
  await page.getByRole('button',{name:'Copy join command',exact:true}).click();
  await expect(page.locator('#copy-status')).toHaveText('Copied');
  expect(await page.evaluate(()=>navigator.clipboard.readText())).toBe(hostedJoin);
  expect(paths.sort()).toEqual(['/api/connections',scoped('connection'),scoped('join')].sort());
  await page.getByRole('link',{name:'Nodes',exact:true}).click();
  await expect(page.getByLabel('Filter by status')).toBeVisible();
  await expect(page.getByRole('button',{name:/flm-worker-01/})).toBeVisible();
  await expect(page.locator('#join-command')).toHaveCount(0);
});
test('settings does not offer unavailable or expired join commands',async({page})=>{
  await fixture(page,{joinFail:true});await page.goto('/#/settings');
  await expect(page.getByRole('region',{name:'Join a node'})).toContainText('Node join is not configured.');
  await expect(page.getByRole('button',{name:'Copy join command',exact:true})).toHaveCount(0);
  await page.unroute('**/api/**');await fixture(page,{joinExpired:true});
  await page.getByRole('button',{name:'Refresh data',exact:true}).click();
  await expect(page.getByRole('region',{name:'Join a node'})).toContainText('Join command unavailable.');
  await expect(page.locator('#join-command')).toHaveCount(0);
});
test('settings shows the full configuration supplied by the live preview',async({page})=>{
  await page.addInitScript(configuration=>{window.FLAME_CLUSTER_CONFIGS={training:configuration}},connection.configuration);
  await fixture(page,{connections:[{...connection,configuration:undefined}]});await page.goto('/#/settings');
  await expect(page.locator('#cluster-config')).toHaveText(JSON.stringify(connection.configuration,null,2));
  expect(JSON.parse(await page.locator('#cluster-config').textContent())).toEqual(connection.configuration);
});
test('settings explains missing cluster configuration instead of inventing metadata JSON',async({page})=>{
  await fixture(page,{connections:[{...connection,configuration:undefined}]});await page.goto('/#/settings');
  await expect(page.locator('#cluster-config')).toHaveText('Cluster configuration is unavailable.');
});
test('resource pagination and filters use upstream pages and reset page one',async({page})=>{
  await fixture(page);await page.goto('/#/applications');
  await page.getByRole('button',{name:'Next →',exact:true}).click();
  await expect(page.locator('#page-number')).toHaveText('Page 2 of 2');
  await expect(page.getByRole('button',{name:/python-12/})).toBeVisible();
  const request=page.waitForRequest(r=>new URL(r.url()).pathname===scoped('applications')&&new URL(r.url()).searchParams.get('state')==='Disabled');
  await page.getByLabel('Filter by status').selectOption('Disabled');
  expect(new URL((await request).url()).searchParams.get('page')).toBe('1');
  await expect(page.getByRole('button',{name:/flmrun/})).toBeVisible();
  await expect(page.getByRole('button',{name:/flmping/})).toHaveCount(0);
});
test('compact desktop layout keeps a full resource page visible',async({page})=>{
  await page.setViewportSize({width:1440,height:900});
  await fixture(page);await page.goto('/#/applications');
  const rows = page.locator('#content tbody tr');
  await expect(rows).toHaveCount(12);
  const last = await rows.last().boundingBox();
  expect(last.y + last.height).toBeLessThan(900);
  await expect(page.getByRole('button',{name:'Next →',exact:true})).toBeInViewport();
});
test('nodes show overall readiness and capacity with service status in details',async({page})=>{
  await fixture(page);await page.goto('/#/nodes');
  await expect(page.locator('#content th')).toHaveText(['Node','Status','Capacity','Address','Runtime','Executor limit','Heartbeat']);
  await expect(page.locator('#content tbody').getByRole('img',{name:'Ready',exact:true})).toBeVisible();
  await expect(page.locator('#content tbody td:nth-child(3) .capacity-box')).toHaveText(['GPU0','CPU8','MEM16 GiB']);
  await expect(page.locator('#content tbody .ready-circle')).toHaveClass(/good/);
  await page.getByRole('button',{name:/flm-worker-01/}).click();
  await expect(page.getByRole('dialog')).toContainText('training-ns');
  await expect(page.getByRole('dialog')).not.toContainText('training-1');
  await expect(page.getByRole('dialog')).toContainText('ready-worker-hash');
  await expect(page.getByRole('dialog').locator('table').first().locator('tbody tr td:nth-child(2)')).toHaveText(['Active','Active']);
  await expect(page.getByRole('dialog').getByRole('cell',{name:'Executor manager',exact:true})).toBeVisible();
  await expect(page.getByRole('dialog').getByRole('heading',{name:'PCI devices'})).toBeVisible();
  await expect(page.getByRole('dialog').getByRole('row').filter({hasText:'0000:03:00.0'}).getByRole('cell')).toHaveText(['0000:03:00.0','10de','1234','030000','-','0']);
  await expect(page.getByRole('dialog')).toContainText('Worker registered and services active');
  await expect(page.getByRole('dialog')).toContainText('192.168.3.8');
  const capacity=page.getByRole('dialog').getByRole('region',{name:'Capacity',exact:true});
  await expect(capacity.locator('dt')).toHaveText(['Total GPU','Total CPU','Total memory']);
  await expect(capacity.locator('dd')).toHaveText(['0','8','16 GiB']);
});
test('session task drawer retains task paging and progress',async({page})=>{
  await fixture(page);await page.goto('/#/sessions');
  await expect(page.getByRole('progressbar').first()).toHaveAttribute('aria-valuenow','76');
  await page.getByRole('button',{name:'session-42',exact:true}).click();
  await expect(page.getByRole('dialog')).toContainText('task-1');
  await page.getByRole('button',{name:'Next tasks →'}).click();
  await expect(page.getByRole('dialog')).toContainText('task-2');
  await expect(page.getByRole('button',{name:'Next tasks →'})).toBeDisabled();
});
test('session commands use direct FLM actions and update available commands',async({page})=>{
  await fixture(page);await page.goto('/#/sessions');page.on('dialog',dialog=>dialog.accept());
  const mutation=page.waitForRequest(r=>r.method()==='POST'&&new URL(r.url()).pathname===scoped('sessions/session-42/actions/close'));
  await page.getByRole('button',{name:'Close',exact:true}).click();await mutation;
  await expect(page.locator('#content').getByText('Closed',{exact:true})).toBeVisible();
  await expect(page.getByRole('button',{name:'Delete',exact:true})).toBeVisible();
  await page.getByRole('button',{name:'Delete',exact:true}).click();
  await expect(page.getByRole('heading',{name:'No sessions yet'})).toBeVisible();
});
test('mutation errors remain visible and preserve session state',async({page})=>{
  await fixture(page,{actionFail:true});await page.goto('/#/sessions');
  await page.getByRole('button',{name:'Suspend',exact:true}).click();
  await expect(page.locator('#notice')).toContainText('Session state changed');
  await expect(page.locator('#content').getByText('Open',{exact:true})).toBeVisible();
});
test('upstream failure is visible without fake healthy empty inventories',async({page})=>{
  await fixture(page,{fail:true});await page.goto('/');
  await expect(page.locator('#notice')).toContainText('FLM is unavailable');
  await expect(page.getByRole('heading',{name:'Unable to load dashboard'})).toBeVisible();
  await expect(page.getByRole('heading',{name:'No nodes yet'})).toHaveCount(0);
});
test('invalid resource hashes normalize to dashboard',async({page})=>{
  await fixture(page);await page.goto('/#/unknown');
  await expect(page).toHaveURL(/#\/dashboard$/);
  await expect(page.getByRole('heading',{name:'Dashboard',exact:true,level:1})).toBeVisible();
});

test('resource navigation remains usable on a narrow screen',async({page})=>{
  await page.setViewportSize({width:390,height:844});await fixture(page);await page.goto('/');
  await page.getByRole('link',{name:'Executors',exact:true}).click();
  await expect(page.getByRole('heading',{name:'Executors',exact:true,level:1})).toBeVisible();
  await expect(page.getByRole('button',{name:'executor-01',exact:true})).toBeVisible();
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
});

const production = {id:'production',namespace:'production-ns',endpoint:'https://prod-flm:8080',displayName:'production-ns/production',clientConfigAvailable:true,configuration:clusterConfiguration('production-1','https://production-1-session-manager:8080')};
const productionInventory = {
  ...inventory,
  applications:[app('production-service')],
  nodes:[{...inventory.nodes[0],metadata:{name:'prod-worker',namespace:'production-ns'},spec:{...inventory.nodes[0].spec,clusterRef:'production-1',address:'192.168.3.20',shims:['cri'],maxExecutors:24},status:{...inventory.nodes[0].status,capacity:{gpu:2,cpu:24,memory:64*1024**3},hostname:'prod-worker'}}],
  sessions:[{...inventory.sessions[0],metadata:{id:'prod-session'}}],
};
const multiple = {connections:[{...connection,namespace:'training-ns'},production],inventories:{production:productionInventory},taskPrefix:true};
function deferred() {
  let resolve;
  const promise=new Promise(done=>{resolve=done});
  return {promise,resolve};
}
async function ignoreTrainingAbort(page) {
  await page.addInitScript(()=>{
    const original=window.fetch;
    window.fetch=(input,options)=>original(input,String(input).includes('/api/connections/training/') ? {...options,signal:undefined} : options);
  });
}
test('configured Flames switch resources and reset pagination, filters, caches, and drawers',async({page})=>{
  await fixture(page,multiple);await page.goto('/#/applications');
  const selector=page.getByRole('combobox',{name:'Cluster',exact:true});
  await expect(selector.locator('option')).toHaveText(['training-ns','production-ns']);
  await expect(selector).toHaveValue('training');
  await page.getByRole('button',{name:'Next →',exact:true}).click();
  await expect(page.getByRole('button',{name:/python-12/})).toBeVisible();
  await selector.selectOption('production');
  await expect(page.getByRole('button',{name:/production-service/})).toBeVisible();
  await expect(page.locator('#page-number')).toHaveText('Page 1 of 1');
  await expect(page.getByRole('button',{name:'Next →',exact:true})).toBeDisabled();
  await selector.selectOption('training');
  await expect(page.getByRole('button',{name:/flmping/})).toBeVisible();
  await expect(page.locator('#page-number')).toHaveText('Page 1 of 2');
  await page.getByLabel('Filter by status').selectOption('Disabled');
  await expect(page.getByRole('button',{name:/flmrun/})).toBeVisible();
  await selector.selectOption('production');
  await expect(page.getByLabel('Filter by status')).toHaveValue('');
  await expect(page.getByRole('button',{name:/production-service/})).toBeVisible();
  await page.getByRole('link',{name:'Sessions',exact:true}).click();
  await page.getByRole('button',{name:'prod-session',exact:true}).click();
  await expect(page.getByRole('dialog')).toContainText('production-task-1');
  await page.getByRole('button',{name:'Next tasks →'}).click();
  await expect(page.getByRole('dialog')).toContainText('production-task-2');
  await page.getByRole('button',{name:'Close details'}).click();
  await selector.selectOption('training');
  await expect(page).toHaveURL(/#\/sessions$/);
  await expect(page.getByRole('button',{name:'session-42',exact:true})).toBeVisible();
  await expect(page.getByRole('dialog')).not.toBeVisible();
  await page.getByRole('button',{name:'session-42',exact:true}).click();
  await expect(page.getByRole('dialog')).toContainText('training-task-1');
  await expect(page.getByRole('dialog')).not.toContainText('production-task');
});
test('Settings clears old join credentials immediately when switching Flames',async({page})=>{
  const pending=deferred();
  await fixture(page,{...multiple,joinCommands:{training:'join-training',production:'join-production'},wait:{'production/join':pending.promise}});
  await page.goto('/#/settings');
  await expect(page.locator('#join-command')).toHaveText('join-training');
  const requested=page.waitForRequest(request=>new URL(request.url()).pathname==='/api/connections/production/join');
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');await requested;
  await expect(page.locator('#join-command')).toHaveCount(0);
  await expect(page.locator('#content')).not.toContainText('join-training');
  pending.resolve();
  await expect(page.locator('#join-command')).toHaveText('join-production');
  await expect(page.locator('#content')).toContainText(production.configuration.cluster.endpoint);
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('training');
  await expect(page.locator('#join-command')).toHaveText('join-training');
});
test('late connection and resource responses cannot overwrite the selected Flame',async({page})=>{
  const pending=deferred();
  await ignoreTrainingAbort(page);
  await fixture(page,{...multiple,wait:{'training/connection':pending.promise,'training/applications':pending.promise}});
  const initial=page.waitForRequest(request=>new URL(request.url()).pathname===scoped('applications'));
  await page.goto('/#/applications');await initial;
  const selector=page.getByRole('combobox',{name:'Cluster',exact:true});
  await expect(selector).toBeEnabled();
  await selector.selectOption('production');
  await expect(page.getByRole('button',{name:/production-service/})).toBeVisible();
  const late=page.waitForResponse(response=>new URL(response.url()).pathname===scoped('applications'));
  pending.resolve();await (await late).finished();
  await expect(page.locator('#eyebrow')).toHaveCount(0);
  await expect(page.getByRole('button',{name:/production-service/})).toBeVisible();
  await expect(page.getByRole('button',{name:/flmping/})).toHaveCount(0);
  await expect(selector).toHaveValue('production');
});
test('switching Flames cancels old task drawers and ignores late task responses',async({page})=>{
  const pending=deferred();
  await ignoreTrainingAbort(page);
  await fixture(page,{...multiple,wait:{'training/tasks':pending.promise}});
  await page.goto('/#/sessions');
  const requested=page.waitForRequest(request=>new URL(request.url()).pathname===scoped('sessions/session-42/tasks'));
  await page.getByRole('button',{name:'session-42',exact:true}).click();await requested;
  await page.evaluate(()=>{const selector=document.querySelector('#connection-name');selector.value='production';selector.dispatchEvent(new Event('change'));});
  await expect(page.getByRole('dialog')).not.toBeVisible();
  await page.getByRole('button',{name:'prod-session',exact:true}).click();
  await expect(page.getByRole('dialog')).toContainText('production-task-1');
  const late=page.waitForResponse(response=>new URL(response.url()).pathname===scoped('sessions/session-42/tasks'));
  pending.resolve();await (await late).finished();
  await expect(page.getByRole('dialog')).toContainText('production-task-1');
  await expect(page.getByRole('dialog')).not.toContainText('training-task');
});
test('selected Flame scopes session mutations and old mutations cannot refresh another Flame',async({page})=>{
  const pending=deferred(), posts=[], productionReads=[];
  await ignoreTrainingAbort(page);
  page.on('request',request=>{
    const path=new URL(request.url()).pathname;
    if(request.method()==='POST')posts.push(path);
    if(path==='/api/connections/production/sessions')productionReads.push(path);
  });
  await fixture(page,{...multiple,wait:{'training/suspend':pending.promise}});
  await page.goto('/#/sessions');
  const staleButton=await page.getByRole('button',{name:'Suspend',exact:true}).elementHandle();
  const requested=page.waitForRequest(request=>request.method()==='POST');
  await page.getByRole('button',{name:'Suspend',exact:true}).click();await requested;
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');
  await expect(page.getByRole('button',{name:'prod-session',exact:true})).toBeVisible();
  await expect(page.getByRole('button',{name:'Suspend',exact:true})).toBeEnabled();
  await staleButton.evaluate(button=>button.click());
  expect(posts).toEqual([scoped('sessions/session-42/actions/suspend')]);
  const late=page.waitForResponse(response=>response.request().method()==='POST');
  pending.resolve();await (await late).finished();
  expect(productionReads).toHaveLength(1);
  const mutation=page.waitForRequest(request=>request.method()==='POST'&&new URL(request.url()).pathname==='/api/connections/production/sessions/prod-session/actions/suspend');
  await page.getByRole('button',{name:'Suspend',exact:true}).click();await mutation;
  await expect(page.locator('#content').getByText('Suspending',{exact:true})).toBeVisible();
  expect(posts).toEqual([scoped('sessions/session-42/actions/suspend'),'/api/connections/production/sessions/prod-session/actions/suspend']);
});
for(const [label,options,message] of [
  ['unavailable',{catalogFail:true},'Flame configuration is unavailable.'],
  ['empty',{connections:[]},'No Flame connections are configured.'],
]) test(`an ${label} Flame catalog is explicit and can be retried`,async({page})=>{
  const paths=[];page.on('request',request=>{if(new URL(request.url()).pathname.startsWith('/api/'))paths.push(new URL(request.url()).pathname)});
  await fixture(page,options);await page.goto('/');
  await expect(page.getByRole('heading',{name:'Unable to load Flame connections'})).toBeVisible();
  await expect(page.locator('#notice')).toContainText(message);
  await expect(page.getByRole('combobox',{name:'Cluster',exact:true})).toBeDisabled();
  expect(paths).toEqual(['/api/connections']);
  await page.unroute('**/api/**');await fixture(page);
  await page.getByRole('button',{name:'Refresh data',exact:true}).click();
  await expect(page.getByRole('img',{name:'Nodes: 1 ready, 0 not ready, 1 total'})).toBeVisible();
});

test('the first configured Flame is selected in catalog order',async({page})=>{
  const paths=[];page.on('request',request=>paths.push(new URL(request.url()).pathname));
  await fixture(page,{...multiple,connections:[production,connection]});await page.goto('/');
  await expect(page.getByRole('combobox',{name:'Cluster',exact:true})).toHaveValue('production');
  await expect(page.getByRole('img',{name:'Nodes: 1 ready, 0 not ready, 1 total'})).toBeVisible();
  expect(paths.some(path=>path.startsWith(scoped('')))).toBe(false);
});
test('switching the selector aborts an in-flight session mutation',async({page})=>{
  const pending=deferred();
  await fixture(page,{...multiple,wait:{'training/suspend':pending.promise}});await page.goto('/#/sessions');
  const requested=page.waitForRequest(request=>request.method()==='POST');
  await page.getByRole('button',{name:'Suspend',exact:true}).click();await requested;
  const aborted=page.waitForEvent('requestfailed',{predicate:request=>request.method()==='POST'&&new URL(request.url()).pathname===scoped('sessions/session-42/actions/suspend')});
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');
  await aborted;pending.resolve();
  await expect(page.getByRole('button',{name:'prod-session',exact:true})).toBeVisible();
  await expect(page.locator('#notice')).toBeHidden();
});

test('Settings downloads exact flame.yaml bytes for only the selected namespace',async({page})=>{
  const requests=[];
  page.on('request',request=>{if(new URL(request.url()).pathname.endsWith('/flame.yaml'))requests.push(new URL(request.url()).pathname)});
  await fixture(page,multiple);await page.goto('/#/settings');
  const downloadButton=()=>page.getByRole('button',{name:'Download flame.yaml',exact:true});
  await expect(downloadButton()).toBeEnabled();
  expect(requests).toEqual([]);
  for(const id of ['training','production']) {
    if(id==='production') await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption(id);
    await expect(downloadButton()).toBeEnabled();
    const downloaded=page.waitForEvent('download');
    await downloadButton().click();
    const download=await downloaded;
    expect(download.suggestedFilename()).toBe('flame.yaml');
    expect(await readFile(await download.path())).toEqual(Buffer.from(clientBundle(id)));
    await expect(page.locator('#download-status')).toHaveText('Download started.');
    await expect(page.locator('body')).not.toContainText('PRIVATE-KEY-FIXTURE');
  }
  expect(requests).toEqual([scoped('flame.yaml'),'/api/connections/production/flame.yaml']);
});
test('Settings explains unavailable client configuration without requesting a bundle',async({page})=>{
  const requests=[];page.on('request',request=>{if(new URL(request.url()).pathname.endsWith('/flame.yaml'))requests.push(request.url())});
  await fixture(page,{connections:[{...connection,clientConfigAvailable:false}]});await page.goto('/#/settings');
  await expect(page.getByRole('region',{name:'Client configuration'})).toContainText('Client configuration is unavailable for this Flame.');
  await expect(page.getByRole('button',{name:'Download flame.yaml',exact:true})).toBeDisabled();
  expect(requests).toEqual([]);
});
test('client download failure is friendly, exposes no response content, and can be retried',async({page})=>{
  const options={downloadFail:true},downloads=[];
  page.on('download',download=>downloads.push(download));
  await fixture(page,options);await page.goto('/#/settings');
  const button=page.getByRole('button',{name:'Download flame.yaml',exact:true});
  await button.click();
  await expect(page.locator('#download-status')).toHaveText('Unable to download flame.yaml. Refresh and try again.');
  await expect(page.locator('body')).not.toContainText('PRIVATE-KEY-ERROR-BODY');
  expect(downloads).toEqual([]);
  options.downloadFail=false;
  const downloaded=page.waitForEvent('download');await button.click();
  expect((await downloaded).suggestedFilename()).toBe('flame.yaml');
});
for(const destination of ['another Flame','another view']) test(`a delayed client configuration cannot download after switching to ${destination}`,async({page})=>{
  const pending=deferred(),downloads=[];
  await ignoreTrainingAbort(page);
  page.on('download',download=>downloads.push(download));
  await fixture(page,{...multiple,wait:{'training/flame.yaml':pending.promise}});await page.goto('/#/settings');
  const started=page.waitForRequest(request=>new URL(request.url()).pathname===scoped('flame.yaml'));
  await page.getByRole('button',{name:'Download flame.yaml',exact:true}).click();await started;
  if(destination==='another Flame') {
    await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');
    await expect(page.getByRole('button',{name:'Download flame.yaml',exact:true})).toBeEnabled();
  } else {
    await page.getByRole('link',{name:'Nodes',exact:true}).click();
    await expect(page.getByRole('button',{name:/flm-worker-01/})).toBeVisible();
  }
  const finished=page.waitForResponse(response=>new URL(response.url()).pathname===scoped('flame.yaml'));
  pending.resolve();await (await finished).finished();
  await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
  expect(downloads).toEqual([]);
  await expect(page.locator('body')).not.toContainText('PRIVATE-KEY-FIXTURE');
  if(destination==='another Flame') {
    const downloaded=page.waitForEvent('download');await page.getByRole('button',{name:'Download flame.yaml',exact:true}).click();
    expect(await readFile(await (await downloaded).path())).toEqual(Buffer.from(clientBundle('production')));
  }
});
test('changing the namespace aborts an active client download request',async({page})=>{
  const pending=deferred();
  await fixture(page,{...multiple,wait:{'training/flame.yaml':pending.promise}});await page.goto('/#/settings');
  const started=page.waitForRequest(request=>new URL(request.url()).pathname===scoped('flame.yaml'));
  await page.getByRole('button',{name:'Download flame.yaml',exact:true}).click();await started;
  const aborted=page.waitForEvent('requestfailed',{predicate:request=>new URL(request.url()).pathname===scoped('flame.yaml')});
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');
  await aborted;pending.resolve();
  await expect(page.locator('#download-status')).toBeEmpty();
  await expect(page.getByRole('button',{name:'Download flame.yaml',exact:true})).toBeEnabled();
});

test('enrolled Pending workers appear before any service status and filters use backend readiness',async({page})=>{
  const pending={metadata:{name:'new-worker',namespace:'training-ns'},spec:{clusterRef:'training-1',address:'192.168.3.30',shims:['cri'],maxExecutors:6,pythonVersion:'3.12',manageObjectCache:true},readiness:'Pending'};
  const disabled={...inventory.nodes[0],metadata:{name:'disabled-worker',namespace:'training-ns'},spec:{...inventory.nodes[0].spec,disabled:true},readiness:'Disabled'};
  const stale={...inventory.nodes[0],metadata:{name:'stale-worker',namespace:'training-ns',generation:2},readiness:'Not ready'};
  await fixture(page,{inventories:{training:{...inventory,nodes:[pending,inventory.nodes[0],disabled,stale]}}});await page.goto('/#/nodes');
  const pendingRow=page.locator('#content tbody tr').filter({has:page.getByRole('button',{name:'new-worker',exact:true})});
  await expect(pendingRow.locator('td')).toHaveText(['new-worker','Pending','GPU-CPU-MEM-','192.168.3.30','CRI','6','—']);
  await expect(pendingRow.getByRole('img',{name:'Pending',exact:true})).toHaveAttribute('title','Pending');
  await expect(pendingRow.locator('.ready-circle')).toHaveClass(/bad/);
  await expect(pendingRow.locator('.capacity-box')).toHaveText(['GPU-','CPU-','MEM-']);
  await expect(page.locator('#status-filter option')).toHaveText(['All states','Pending','Ready','Not ready','Disabled','Deleting']);
  await pendingRow.getByRole('button').click();
  const drawer=page.getByRole('dialog');
  await expect(drawer).toContainText('Worker status has not been reported yet.');
  await expect(drawer).toContainText('training-ns');
  await expect(drawer).toContainText('192.168.3.30');
  await expect(drawer).not.toContainText(/Invalid Date/);
  await expect(drawer.getByText('0',{exact:true})).toHaveCount(0);
  await expect(drawer.getByRole('region',{name:'Capacity',exact:true}).locator('dd')).toHaveText(['-','-','-']);
  await expect(drawer.getByRole('region',{name:'Node resource usage'}).locator('dd')).toHaveText(['-','-','-']);
  await page.getByRole('button',{name:'Close details'}).click();
  for(const [readiness,name] of [['Disabled','disabled-worker'],['Not ready','stale-worker'],['Ready','flm-worker-01'],['Pending','new-worker']]) {
    const requested=page.waitForRequest(request=>new URL(request.url()).pathname===scoped('nodes')&&new URL(request.url()).searchParams.get('state')===readiness);
    await page.getByLabel('Filter by status').selectOption(readiness);await requested;
    await expect(page.locator('#content tbody tr')).toHaveCount(1);
    await expect(page.locator('#content tbody tr')).toContainText(name);
    await expect(page.locator('#content .readiness')).toHaveText(readiness);
  }
});
test('switching namespaces shows each FlameWorker address, runtime, and executor limit',async({page})=>{
  await fixture(page,multiple);await page.goto('/#/nodes');
  await expect(page.locator('#content tbody td').nth(3)).toHaveText('192.168.3.8');
  await expect(page.locator('#content tbody td').nth(5)).toHaveText('10');
  await expect(page.locator('#content tbody td').nth(6)).not.toHaveText('—');
  await expect(page.locator('#content')).not.toContainText('Invalid Date');
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');
  await expect(page.locator('#content tbody td').nth(3)).toHaveText('192.168.3.20');
  await expect(page.locator('#content tbody td').nth(4)).toHaveText('CRI');
  await expect(page.locator('#content tbody td').nth(5)).toHaveText('24');
  await page.getByRole('button',{name:/prod-worker/}).click();
  await expect(page.getByRole('dialog')).toContainText('production-ns');
  await expect(page.getByRole('dialog')).not.toContainText('production-1');
  await expect(page.getByRole('dialog')).not.toContainText('training-ns');
});
test('node capacities come from reported status and stay isolated between clusters',async({page})=>{
  await fixture(page,multiple);await page.goto('/#/nodes');
  await expect(page.locator('#content tbody .capacity-box')).toHaveText(['GPU0','CPU8','MEM16 GiB']);
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');
  await expect(page.locator('#content tbody .capacity-box')).toHaveText(['GPU2','CPU24','MEM64 GiB']);
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('training');
  await expect(page.locator('#content tbody .capacity-box')).toHaveText(['GPU0','CPU8','MEM16 GiB']);
});
test('missing or invalid reported capacity uses literal hyphens and ignores unrelated capacity fields',async({page})=>{
  const node=structuredClone(inventory.nodes[0]);delete node.status.capacity;
  node.capacity={gpu:9,cpu:99,memory:99};
  const invalid={...node,metadata:{...node.metadata,name:'invalid-capacity'},status:{...node.status,capacity:{gpu:null,cpu:-1,memory:'unknown'}}};
  await fixture(page,{inventories:{training:{...inventory,nodes:[node,invalid]}}});await page.goto('/#/nodes');
  await expect(page.locator('#content tbody .capacity-box')).toHaveText(['GPU-','CPU-','MEM-','GPU-','CPU-','MEM-']);
});

test('empty Nodes explains enrollment without requiring an FLM registration',async({page})=>{
  await fixture(page,{inventories:{training:{...inventory,nodes:[]}}});await page.goto('/#/nodes');
  await expect(page.getByRole('heading',{name:'No nodes yet'})).toBeVisible();
  await expect(page.locator('#content')).toContainText('Nodes appear here when they enroll.');
  await expect(page.locator('#content')).toContainText('Open Settings for the node join command.');
});

test('cluster labels hide Kubernetes terminology and the internal config name',async({page})=>{
  await fixture(page,multiple);await page.goto('/#/nodes');
  await page.getByRole('combobox',{name:'Cluster',exact:true}).selectOption('production');
  await expect(page.locator('#eyebrow')).toHaveCount(0);
  await expect(page.locator('#content')).not.toContainText('production-1');
  await page.getByRole('button',{name:/prod-worker/}).click();
  await expect(page.locator('#detail-kind')).toHaveText('NODE');
  await expect(page.getByRole('dialog')).not.toContainText('production-1');
  await expect(page.getByRole('dialog').locator('dt').first()).toHaveText('Cluster');
  await expect(page.getByRole('dialog')).not.toContainText(/namespace|Kubernetes|observed generation/i);
  await page.getByRole('button',{name:'Close details'}).click();
  await page.getByRole('link',{name:'Settings',exact:true}).click();
  await expect(page.locator('#cluster-config')).toHaveText(JSON.stringify(production.configuration,null,2));
  expect(JSON.parse(await page.locator('#cluster-config').textContent())).toEqual(production.configuration);
  await expect(page.locator('#content')).not.toContainText('production-ns/production');
  await expect(page.locator('#content')).not.toContainText(/namespace|Kubernetes/i);
});
